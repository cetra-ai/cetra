# Track IN, M6: apps, voice, browser agents, tools and evaluation — plan

**What M6 adds:**
- Captured cases for n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, MCP, OpenLIT, the vector databases through OpenLIT and OpenLLMetry, and Promptfoo.
- Pages for every item the spikes kept: captured, setup only, or merged.
- The normalizer rules those captures need. All are gated on the instrumentation scope (D-IN-5, D-IN-6).

**What the capture tool learns:**
- per-scenario Python environments;
- npm and Python tool environments.

**How to run the phases:**
- The implementer is the GLM 5.3 agent, one per phase (Haiku when GLM has no credit).
- Conventions are M5's:
  - Node on `PATH`;
  - `CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target`;
  - commit with `git commit -m … -- <paths>`;
  - `touch tools/capture/src/main.rs` before the first capture;
  - never `--run-ignored`.
- **Never guess. Stop and report.**

**Read first:**
- `docs/plans/in-integrations/00-design.md`:
  - § Apps, no-code, voice, browser agents, tools and evaluation (M6);
  - D-IN-2, D-IN-5, D-IN-6, D-IN-8, D-IN-15, D-IN-25;
  - the fixture-group rule (the "New fixture group names" bullet near line 84).
- `spikes/m6-apps.md` and `spikes/m6-tools-eval.md`: the runs, span dumps and proposed rules this plan cites by ID.
- `05-m5-gateways.md`: IN5.1, how tools and their environments are fetched; and Amendment B, the LiteLLM Proxy tool environment.
- `docs/spec/02-ingestion.md`.
- `tools/capture/src/{run,scenario,receiver}.rs`, `tools/capture/fetch-tools.sh` and `tools/capture/py/src/capture_py/harness.py`.

## What it is, and what it is not

**When it is done:**
- **Captured cases** (case ID, then group):

  | Case ID | Group |
  |---|---|
  | `n8n/agent-webhook` | `n8n` |
  | `langflow/basic-prompting` | `langflow` |
  | `promptflow/openai-trace` | `promptflow` |
  | `livekit/text-session` | `livekit` |
  | `pipecat/openai-pipeline` | `pipecat` |
  | `genai/browser-use-agent` | `genai` |
  | `genai/mcp-tool` | `genai` |
  | `genai/openlit-chat` | `genai` |
  | `genai/openlit-vector` | `genai` |
  | `openllmetry/vector-databases` | `openllmetry` |
  | `genai/promptfoo-eval` | `genai` |

- **Pages:**
  - n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, MCP, OpenLIT, Vector databases (one page covering Chroma, Qdrant, Milvus, Pinecone and Weaviate), and Promptfoo, all with captured cases;
  - Stagehand, LobeChat, Gradio, Ragas, UQLM, Azure AI Evaluation, Cleanlab, and Harbor and Terminal-Bench, all setup only.
- **Index:** an "Apps and tools" section.

**Not in M6:**
- **Not supported**, recorded in the design's § Not supported:
  - Kernel (browser): it exports logs only;
  - Twilio Agent Connect, the Docker and Vercel sandboxes, Scalekit, Bubble: no traces;
  - Daytona: infrastructure spans only;
  - Langdock and Ragflow: Langfuse only;
  - Trieve: stale;
  - Exa, Tavily, Parallel AI: no instrumentation;
  - Firecrawl: OpenLIT's target is the v1 SDK;
  - Coval: no results API.
  - Morph Reflex and Testable Minds wait for the user's answer (question 3).
- **No capture of** Stagehand (a Chrome extension calls the real OpenAI), LobeChat, Weaviate (both need a server), Pinecone (a fake REST host would be needed), Cleanlab (cloud key), Harbor (Docker), the old MCP SDK through OpenLLMetry, and LiveKit or Pipecat voice spans (STT/TTS/EOU need audio services).
- **No JSON enum names.** Harbor's `harbor-atif2otel` JSON writes enum names, but OTLP/JSON requires integer enums, so cetra stays strict. The Harbor page uses the protobuf export (D-IN-26 item 6).
- **Eval tools are setup only.** Ragas, UQLM and Azure AI Evaluation show "trace the LLM calls, send results with `cetra.score`". Testing their samples would add heavy packages to the SDK's dev group (D-IN-26 item 5).

## Questions for the user (asked 2026-11-24, unanswered)

| # | Question | Blocks |
|---|---|---|
| Q1 | Vector-database `retriever` kind: (a) reads only (query, search, get, fetch), writes stay `span` — recommended; or (b) every operation | IN6.4 steps 2–3, IN6.6 vector cases |
| Q2 | OpenLIT writes `gen_ai.usage.cost = 0` when its price table has no row: (a) a reported cost of exactly 0 on a span with tokens counts as no report, every source — recommended; (b) the same, OpenLIT scopes only; (c) no rule, the page documents `pricing_json` | IN6.4 step 4, IN6.6 OpenLIT cases |
| Q3 | Morph Reflex, Testable Minds: URLs, or not supported | IN6.9 records only |

Phases that do not depend on them run first. The answers are written here, then into D-IN-26.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-5 | Scope-gated kind, provider and content rules |
| D-IN-6 | Plain `input`/`output` read only on a named scope (Pipecat) |
| D-IN-8 | Tiers: captured, hand-written, SDK test, setup only |
| D-IN-15 | Framework names skipped as providers |
| D-IN-25 | Tool binaries and tool environments under `target/capture-tools`, checksummed or locked |
| D-IN-26 (new, recorded by IN6.9) | Items below |

D-IN-26, M6's decisions:

1. **Per-scenario Python environments.**
   - `scenario.json` gains an optional `project` (string, a directory name under `tools/capture/envs/`). A Python scenario with `project` runs with `uv run --project tools/capture/envs/<project> --frozen python main.py`; without it, the runner uses `tools/capture/py` as today.
   - Each environment is a uv project with its own `uv.lock`:
     - `package = false`;
     - `exclude-newer = "7 days"`;
     - `capture-py = { path = "../../py", editable = true }` in `[tool.uv.sources]` and in `dependencies`, so its scenarios import the harness;
     - its scenario modules stay under `tools/capture/py/scenarios/<group>/<case>/`.
   - The lint trio runs in each environment over the scenarios that name it (§ IN6.1).
   - It is used when a case's packages cannot share the capture project's lock (spike findings: OpenLIT needs `openai<3`, mcp 2 conflicts with semantic-kernel).
2. **Tool environments** (servers and CLIs the scenario launches; M5's pattern) are fetched by `just capture-tools` into `target/capture-tools/<name>/<version>/`:
   - **n8n and Promptfoo:** npm packages from `tools/capture/n8n/` and `tools/capture/promptfoo/`. Each holds a `package.json` and a `pnpm-lock.yaml`, with `minimumReleaseAge` as the repo's other pnpm projects. They install with `pnpm install --frozen-lockfile --dir <project> --modules-dir <target dir>/node_modules`.
   - **Langflow:** a uv project `tools/capture/langflow/`, like `litellm-proxy`.
   - **Browser Use** needs Chromium. `fetch-tools.sh` installs it with the Playwright version the Browser Use environment locks: `PLAYWRIGHT_BROWSERS_PATH=target/capture-tools/playwright uv run --project tools/capture/envs/browser-use --frozen playwright install chromium`.
3. **New fixture groups:** `n8n`, `livekit` and `pipecat`, which the design already lists, plus `promptflow` and `langflow`. Browser Use, MCP, OpenLIT and Promptfoo go in `genai`; the OpenLLMetry vector case goes in `openllmetry`.
4. **Pages with no capture:**
   - setup only: Stagehand, LobeChat, Weaviate (a section of the Vector databases page), Pinecone (the same page), Cleanlab, Harbor and Terminal-Bench;
   - merged: Gradio, a setup-only page pointing at the instrumentors of the client library.
5. **Eval tools.** Ragas, UQLM and Azure AI Evaluation get setup-only pages, not SDK-test pages. Their samples are not run.
6. **OTLP/JSON enum names stay rejected.** The Harbor page uses `harbor trace export --encoding pb` and a POST of each file.
7. **The `mcp` registry row stays** (OpenInference's `MCPInstrumentor`). It joins an `mcp<2` server's spans to the client trace; the page says it records no spans of its own, and that `mcp>=2` traces natively.
8. **Rules.** Items 8 to 10 are recorded with Q1–Q3's answers.

## Versions

Every version is the newest upload on or before the 7-day cutoff of the day the phase runs. The spike's values are at the cutoff of 2026-11-17:

| Package | Spike version | Where |
|---|---|---|
| `n8n` (npm) | 2.48.4 | `tools/capture/n8n/` |
| `promptfoo` (npm) | 0.125.1 | `tools/capture/promptfoo/` |
| `langflow` | 1.13.2 | `tools/capture/langflow/` |
| `promptflow-tracing`, `promptflow-devkit` | 1.18.5 | capture project, or its own environment if the lock conflicts |
| `livekit-agents`, `livekit-plugins-openai` | 1.8.12 | the same |
| `pipecat-ai[openai]` | 1.16.0 | the same |
| `browser-use` | 1.0.6 | `envs/browser-use` |
| `mcp` | 2.3.0 | `envs/mcp` |
| `openlit` | 1.45.0, with `openai<3` | `envs/openlit` |
| `chromadb`, `qdrant-client`, `pymilvus[milvus_lite]` | 1.5.9, 1.20.0, 3.0.3 | `envs/openlit` and `envs/vector` |
| `opentelemetry-instrumentation-{chromadb,qdrant}` | 0.63.1 | `envs/vector` |
| `opentelemetry-instrumentation-milvus` | 0.60.0, because 0.63.1 cannot install (spike finding 3) | `envs/vector` |

The implementer:
- runs `uv lock`, `npm view` or `pnpm` resolution on the day;
- records the versions it used;
- stops on any version that differs from the spike's in its major or minor number, or that will not install.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN6.1 Capture tool | `tools/capture/src/{run,scenario,receiver}.rs`, `tools/capture/fetch-tools.sh`, `justfile`, new `tools/capture/{n8n,promptfoo,langflow}/` and `tools/capture/envs/{browser-use,mcp,openlit,vector}/` (lock files only; scenarios come later), `crates/otel/tests/support/mod.rs` and `fixtures/otlp/README.md` (groups), `website/scripts/check-backing.mjs` (group regex, if needed), `AGENTS.md` | — |
| IN6.2 Voice rules | `crates/otel/src/{kind.rs,conventions/genai.rs,context.rs,…}`, `docs/spec/02-ingestion.md` | — |
| IN6.3 App rules | `crates/otel/src/{kind.rs,conventions/*}`, a new convention module if needed, `docs/spec/02-ingestion.md` | IN6.2 (same files) |
| IN6.4 OpenLIT, vector and Promptfoo rules | `crates/otel/src/{kind,cost}.rs`, `conventions/*`, `docs/spec/02-ingestion.md` | IN6.3; Q1, Q2 |
| IN6.5 Captures A | scenarios + fixtures: promptflow, livekit, pipecat, browser-use, mcp | IN6.1, IN6.3 |
| IN6.6 Captures B | scenarios + fixtures: openlit-chat, openlit-vector, openllmetry vector | IN6.1, IN6.4 |
| IN6.7 Captures C | scenarios + fixtures: n8n, langflow, promptfoo | IN6.1, IN6.4 |
| IN6.8 Pages | `website/src/content/docs/docs/integrations/*.mdx` | IN6.5–IN6.7 for the captured pages; the setup-only pages may start after IN6.1 |
| IN6.9 Index, sidebar, records | as IN5.5 | IN6.8 |

**Parallel runs:**
- IN6.1 and IN6.2 have disjoint files and can run at the same time.
- The setup-only half of IN6.8 can run beside any phase.
- IN6.5 needs IN6.3 only for blessing. It may capture to the scratchpad first, as M5's IN5.3 did.

---

## IN6.1 — Capture tool

**Goal:** scenarios can name their own Python environment and launch npm and Python tool environments. The new groups exist.

**Steps.**
1. **`scenario.rs`:**
   - `Scenario` gains `#[serde(default)] pub project: Option<String>`.
   - Validation: the value matches `^[a-z0-9][a-z0-9-]*$`, and `tools/capture/envs/<project>/pyproject.toml` exists. A TypeScript scenario with `project` is an error.
   - The validation error variants follow the file's existing ones.
   - Tests: a valid project; an unknown directory; a bad name; a TypeScript scenario.
2. **`run.rs`:**
   - `Program::for_language` takes the scenario's `project`. With `Some(p)`, Python uses `tools/capture/envs/<p>` instead of `tools/capture/py`.
   - Update every caller and the program tests.
3. **`GROUPS`:** add `n8n`, `livekit`, `pipecat`, `promptflow` and `langflow` to the three allow-lists: `tools/capture/src/scenario.rs`, `crates/otel/tests/support/mod.rs` and `fixtures/otlp/README.md` § Layout.
   - Read `website/scripts/check-backing.mjs`. If its fixture regex does not accept these names, widen it to `[a-z0-9_]+` (the design's note), with a test in its `.test.mjs`.
4. **`receiver.rs`, chunked bodies:**
   - Add the test `a_chunked_otlp_body_is_received`: a POST with `Transfer-Encoding: chunked` and no `Content-Length` carries a protobuf `ExportTraceServiceRequest`, and the spans are recorded.
   - If it fails, fix the receiver. If the fix is not local to `receiver.rs`, stop and report.
5. **Environments** (lock files and project files only, no code). Each Python environment depends on `capture-py` as in D-IN-26 item 1, plus `openai` and the `opentelemetry-*` pins `tools/capture/py` uses, unless a package caps them. Report each cap.

   | Directory | Content |
   |---|---|
   | `tools/capture/envs/browser-use/` | `browser-use`, `opentelemetry-sdk`, `opentelemetry-exporter-otlp-proto-http`, `pydantic-ai` as `browser-use` pins it |
   | `tools/capture/envs/mcp/` | `mcp==2.3.0` |
   | `tools/capture/envs/openlit/` | `openlit==1.45.0`, `openai<3` (the newest 2.x at the cutoff), `chromadb`, `qdrant-client`, `pymilvus[milvus_lite]` at § Versions |
   | `tools/capture/envs/vector/` | the three vector clients, `opentelemetry-instrumentation-{chromadb,qdrant}==0.63.1`, `opentelemetry-instrumentation-milvus==0.60.0` |
   | `tools/capture/langflow/` | `langflow`, a tool environment like `tools/capture/litellm-proxy` |
   | `tools/capture/n8n/`, `tools/capture/promptfoo/` | `package.json` (exact version, `"private": true`), `pnpm-workspace.yaml` with the repo's `minimumReleaseAge`, `pnpm-lock.yaml` |

   Each Python environment's `pyproject.toml` carries the capture project's `[tool.ruff]` and `[tool.mypy]` sections, so the lint trio runs there.
6. **`fetch-tools.sh`:** after the LiteLLM Proxy environment, install, each skipped when its lock copy matches (M5's rule):
   - Langflow, into `target/capture-tools/langflow/<version>`, as LiteLLM;
   - n8n and Promptfoo: `pnpm install --frozen-lockfile --dir tools/capture/<name> --modules-dir target/capture-tools/<name>/<version>/node_modules`;
   - every `tools/capture/envs/*` environment: `uv sync --frozen --project tools/capture/envs/<name>`, which writes its `.venv` in place;
   - Chromium, as D-IN-26 item 2, skipped when `target/capture-tools/playwright` holds a `chromium-*` directory.
7. **`justfile`, `check-capture`:** the lint trio also runs in each `tools/capture/envs/*` environment, over the scenario directories whose `scenario.json` names it.
   - `tools/capture/py`'s mypy config excludes those directories, so they are not checked twice against the wrong environment.
   - The exact selection mechanism (a small script, or `find` + `jq`) is the implementer's choice inside the `justfile`. Say which.
8. **`AGENTS.md`:** the capture-tools row and the Required tools line name the new environments. A new line in the Tests section says: "a capture scenario whose packages cannot share `tools/capture/py`'s lock sets `project` in its `scenario.json` (D-IN-26)".

**Done when:**
- `cargo nextest run -p cetra-capture`, plus clippy and fmt for it;
- `cargo nextest run -p cetra-otel --test fixtures`, for the groups;
- `just capture-tools`, twice; the second run installs nothing;
- the lint trio in `tools/capture/py` and in each environment;
- `node --test website/scripts/` if `check-backing` changed;
- `bash -n tools/capture/fetch-tools.sh`.

**Commit:** `Let capture scenarios use their own environments`

## IN6.2 — Voice rules (Pipecat, LiveKit)

**Goal:** rules R-PC-1 to R-PC-3 and R-LK-1 to R-LK-3 of `spikes/m6-apps.md`, as written there, each with unit tests. Spec 02 changes in the same commit.

**Steps.** For each rule:
- read the spike's dump in § 4 (LiveKit) or § 5 (Pipecat);
- find the existing mechanism that does the same thing for another scope (Mirascope or Agent Framework in `kind.rs`'s `scope_gated`; VoltAgent's plain values; D-IN-2's endpoint rule in `context.rs`);
- reuse that mechanism;
- name it in the report.

1. **R-PC-2** (global kind table, `genai_operation`): `tts` and `stt` give `span`. No other realtime value is added.
2. **R-PC-3** (scope `pipecat.turn`): span names `conversation` and `turn` give `chain`. On those spans, the session comes from `conversation.id`. Use the session mechanism of M4 (`gen_ai.conversation.id`); name it.
3. **R-PC-1** (scope starts with `pipecat`): plain `input` and `output` are the span's input and output, through the D-IN-6 path that VoltAgent's plain values use.
   - An `input` that parses as an OpenAI message array is converted.
   - Any other value is the plain value (rule 10), with no error.
   - `output` is text, or the plain value.
   - On any other scope, plain `input`/`output` stay unread (test).
4. **R-LK-1** (scope `livekit-agents`): a `gen_ai.provider.name` containing `.` or `:` is a host. It feeds D-IN-2's endpoint lookup (`ENDPOINT_PROVIDERS`) and is not the provider.
   - A mapped host gives its provider.
   - An unmapped host gives an empty provider, never the host string.
   - Test: `api.openai.com` gives `openai`; `127.0.0.1:1234` gives an empty provider.
5. **R-LK-2** (scope `livekit-agents`):
   - span name `llm_node` → `chain`;
   - `llm_request_run` and `start_agent_activity` → `span`;
   - `llm_request` stays `llm`.
6. **R-LK-3** (scope `livekit-agents`): content, only where the GenAI content keys are absent:
   - `agent_turn`: input `lk.pii.user_input`, output `lk.pii.response.text`;
   - `llm_node`: input `lk.pii.chat_ctx` (stored as JSON, the plain value), output `lk.pii.response.text`;
   - `function_tool`: `lk.pii.function_tool.arguments` and `.output`, used only when `gen_ai.tool.call.arguments` / `.result` are absent.

   Read keys are consumed. TTFT comes from `gen_ai.response.time_to_first_chunk` (seconds), added to the TTFT list for scope `livekit-agents` only. Find the TTFT list and name it.

**Tests:**
- each rule;
- the same attributes on another scope stay unchanged;
- a LiveKit trace built from the spike's dump (§ 4, the span list) gives exactly one `llm` per model call.

**Done when:** `cargo nextest run -p cetra-otel`, clippy and fmt pass, with no `expected.json` changed (stop if one changes).

**Commit:** `Read Pipecat and LiveKit spans`

## IN6.3 — App rules (Prompt Flow, Langflow, n8n)

**Goal:** rules R-PF-1 to R-PF-3, R-LF-1 to R-LF-2 and R-N8N-1 to R-N8N-3 of `spikes/m6-apps.md` §§ 1–3, as written there. Spec 02 changes in the same commit. Same method as IN6.2: reuse the existing mechanism and name it.

**Steps.**
1. **Prompt Flow** (scope `promptflow`):
   - Kind from `span_type`:
     - `LLM` → `llm`;
     - `Embedding` → `embedding`;
     - `Retrieval` → `retriever`;
     - `Flow` and `LangChain` → `chain`;
     - `Function` → `span`.
   - Model and tokens:
     - response model from `llm.response.model`;
     - request model from the `model` field of the `inputs` JSON;
     - tokens from `llm.usage.prompt_tokens`, `llm.usage.completion_tokens` and `llm.usage.total_tokens`.
   - IO: input from `inputs`, output from `output`.
     - On an `LLM` span, `inputs` holds the OpenAI request parameters, including `messages`. Use the OpenAI message converter on `messages` when it is present; otherwise the plain value.
     - `output` is the OpenAI response object. Read its `choices[0].message` through the converter; otherwise the plain value.
     - Before writing this, check what the dump holds; if it differs, stop.
   - The OpenInference detection (`llm.` prefix) must not also claim these spans' IO. Check `detect.rs`, and write a test with both conventions.
2. **Langflow** (scope `langflow`):
   - the root span (`flow_id` present) → `chain`;
   - component spans by `trace_type`: `agent` → `agent`, `tool` → `tool`, `retriever` → `retriever`, `embedding` → `embedding`, anything else (including `llm` and `prompt`) → `chain`;
   - input from `inputs`, output from `outputs`, both stored as the plain value.
3. **n8n** (scope `n8n-workflow`):
   - `workflow.execute` → `chain`;
   - `node.execute` without `gen_ai.operation.name` → `span`;
   - with an operation, the existing GenAI rules decide.
   - Display name: when the span name is `node.execute`, the stored span name is `n8n.node.name`; for `workflow.execute`, it is `n8n.workflow.name`.
   - Find where the stored span name is set. If no rule renames spans today, this is a new mechanism: stop and report before writing it.

**Tests:** each rule; the same keys on another scope are unchanged.

**Done when:** as IN6.2.

**Commit:** `Read Prompt Flow, Langflow and n8n spans`

## IN6.4 — OpenLIT, vector and Promptfoo rules

**Blocked until Q1 and Q2 are answered.** Steps 1, 5 and 6 do not depend on them, but the phase commits once.

**Steps.**
1. **R-OL-1** (scope starts with `openlit.instrumentation.`): when `gen_ai.operation.name` is not a value of the `genai_operation` table, `gen_ai.operation.type` decides: `invoke_agent` → `agent`, `execute_task` → `chain`.
2. **OpenLIT vector spans** (scope starts with `openlit.instrumentation.`, `gen_ai.operation.name = vectordb`):
   - (Q1 a) `retriever` when `db.operation.name`, lowercased, is `get`, `query` or `search`; `span` otherwise;
   - (Q1 b) `retriever` always.
3. **OpenLLMetry vector spans:**
   - Scopes: `opentelemetry.instrumentation.chromadb`, `.qdrant`, `.milvus`, `.pinecone` and `.weaviate`.
   - (Q1 a) these span names give `retriever`, a fixed table in `kind.rs`:
     - Chroma: `chroma.query`, `chroma.get`;
     - Qdrant: `qdrant.query_points`, `qdrant.search`, `qdrant.scroll`;
     - Milvus: `milvus.search`, `milvus.query`, `milvus.get`;
     - Pinecone: `pinecone.query`, `pinecone.fetch`;
     - Weaviate: names starting with `db.weaviate.collections.query.` or `db.weaviate.gql.query.`.
   - (Q1 b) every span on those scopes gives `retriever`.
4. **Zero reported cost:**
   - (Q2 a) in `cost.rs` step 1, a reported total of exactly `0.0` on a span with input or output tokens above 0 is treated as no report, and step 2 prices it;
   - (Q2 b) the same, only when the scope starts with `openlit`;
   - (Q2 c) no code; the page documents it.

   Spec 02 § Cost and D109's text change for (a) and (b).
5. **R-OL-3:** `browser_use` joins the framework-name skip table of D-IN-15.
6. **R4, Promptfoo** (scope `promptfoo.providers`): a span with `gen_ai.evaluation.name` is `evaluator`. Read `spikes/m6-tools-eval.md` § Promptfoo for the keys. The request and response bodies are not read.

**Tests:** each rule; another scope is unchanged; the Q-dependent cases as answered.

**Done when:** as IN6.2.

**Commit:** `Read OpenLIT, vector database and Promptfoo spans`

## IN6.5 — Captures A: Prompt Flow, LiveKit, Pipecat, Browser Use, MCP

**Goal:** five captured cases, each written from the spike's sketch:
- `spikes/m6-apps.md`: § 3 (Prompt Flow), § 4 (LiveKit), § 5 (Pipecat), § 6 (Browser Use 1.0);
- `spikes/m6-tools-eval.md`: § MCP, native case.

The spike scripts live in the scratchpad and may be gone. The dumps in the spike files are the reference.

**Steps.**
1. **Environments:**
   - Add Prompt Flow, LiveKit and Pipecat to `tools/capture/py` and run `uv lock`. Report every moved pin.
   - If the lock refuses one, give that case its own environment under `tools/capture/envs/<name>`, as in IN6.1 step 5, and report it.
   - Browser Use uses `envs/browser-use`; MCP uses `envs/mcp`.
2. **Scenarios.** Each uses the mock (`OPENAI_BASE_URL` or the library's `base_url`) and the runner's OTLP environment, and does what the spike sketch says:

   | Case | Program |
   |---|---|
   | `promptflow/openai-trace` | Two `@trace` functions around two chat calls (a tool call, then text); `setup_exporter_from_environ()` with `OTEL_EXPORTER_OTLP_ENDPOINT` set to the full traces URL (Prompt Flow posts to it as given) |
   | `livekit/text-session` | `AgentSession` text mode with one `get_weather` function tool and `set_tracer_provider(provider, allow_pii=True)` |
   | `pipecat/openai-pipeline` | The aggregator pipeline with `OpenAILLMService`, a registered `get_weather`, `enable_tracing=True` and `conversation_id` |
   | `genai/browser-use-agent` | `browser-use` 1.0 with `pydantic_ai.Agent.instrument_all(True)` and plain OTel; headless Chromium from `PLAYWRIGHT_BROWSERS_PATH=<CETRA_CAPTURE_TOOLS>/playwright`; the mock returns the `done` JSON; `ANONYMIZED_TELEMETRY=false`, `BROWSER_USE_CLOUD_SYNC=false` |
   | `genai/mcp-tool` | An in-process `MCPServer` with `get_weather`; a chat turn whose tool call is executed through the MCP client; the final turn |

   The mock turns follow existing tool-call scenarios, such as `openinference/openai-agents-tool`. Volatile and timing attributes are found by two runs, as in M5.
3. **Capture, bless, check.**
   - Commands: `just capture …`, `just fixtures-bless`, `just fixtures-fmt`, `just fixtures-check`.
   - Expected kinds come from the spike, adjusted by IN6.2 and IN6.3:
     - Prompt Flow: `span`, `llm` ×2, `span`;
     - LiveKit: `chain` (session), `span`, `agent`, `chain` (`llm_node`), `llm`, `span`, `tool`, with exactly two `llm`;
     - Pipecat: `chain` ×2, `llm` ×2 with input and output;
     - Browser Use: `agent`, `llm`, `tool`;
     - MCP: `llm` ×2, `tool`, `span`.
   - Every `llm` with tokens is priced (`golden.rs`), and there is no `cetra.normalize_error`.
   - Stop and report any deviation.

**Done when:**
- `cargo nextest run -p cetra-otel --test golden --test fixtures`;
- `cargo run -q -p cetra-capture -- run <the five cases> --check`;
- the lint trio in each environment used.

**Commit:** `Capture Prompt Flow, LiveKit, Pipecat, Browser Use and MCP`

## IN6.6 — Captures B: OpenLIT and the vector databases

**Blocked until IN6.4 is committed.**

**Steps.** Write the scenarios from `spikes/m6-tools-eval.md` § OpenLIT, § Vector databases and § Capture plan.

| Case | Environment | Program |
|---|---|---|
| `genai/openlit-chat` | `envs/openlit` | `openlit.init(otlp_endpoint=…, disable_metrics=True, capture_message_content=True, disabled_instrumentors=["httpx","requests","urllib3","aiohttp"])`; one chat with a tool call, the final turn, and one embedding call. The pricing file is unreachable; that is the Q2 case |
| `genai/openlit-vector` | `envs/openlit` | add, query, then delete on Chroma (in-memory), Qdrant (`:memory:`) and Milvus Lite (a temp file) |
| `openllmetry/vector-databases` | `envs/vector` | the same three operations on each, through OpenLLMetry's instrumentors |

- Add `openlit.agent.version_hash` to `volatile_attributes` (spike finding 9).
- Expected kinds:
  - `openlit-chat`: `llm` ×2, `embedding`, cost as answered by Q2;
  - vector cases: per Q1.

**Done when:** as IN6.5, for these three cases.

**Commit:** `Capture OpenLIT and vector databases`

## IN6.7 — Captures C: n8n, Langflow, Promptfoo

**Steps.** Write the scenarios from `spikes/m6-apps.md` §§ 1–2 and `spikes/m6-tools-eval.md` § Promptfoo. Each is a Python scenario in `tools/capture/py` that launches a tool from `target/capture-tools` with `harness.tool_binary`, as M5's gateways do. Use `free_port`, `wait_http(proc=…)` and `stop_gateway`.

1. **`n8n/agent-webhook`:**
   - Workflow: Webhook, then AI Agent, then OpenAI Chat Model, with the `openAiApi` credential's `url` on the mock.
   - Setup: `n8n import:workflow` and `import:credentials` from files in the scenario directory, then `update:workflow --active=true`, then `n8n start`.
   - Environment: `N8N_USER_FOLDER` (a temp dir), `N8N_DIAGNOSTICS_ENABLED=false`, `N8N_VERSION_NOTIFICATIONS_ENABLED=false`, `N8N_TEMPLATES_ENABLED=false`, `N8N_AI_ENABLED=false`, the `N8N_OTEL_*` of the spike, `N8N_OTEL_TRACES_PRODUCTION_ONLY=false`, and `N8N_PORT` from `free_port()`.
   - Request: one `POST /webhook/<path>`.
   - Wait for the export, then stop.
   - Expected: `chain`, `span`, `agent`, `llm`. Names come from `n8n.node.name`. There is no content.
2. **`langflow/basic-prompting`:**
   - Run the Basic Prompting starter flow with the model set, as the spike did, through `POST /api/v1/run/<id>`.
   - Langflow rewrites `127.0.0.1` when it thinks it runs in a container. If the mock URL must use another loopback name, the mock needs a second bind address, which is a runner change. **Stop and report** before changing the runner.
   - Expected:
     - trace A: `chain` root and components;
     - trace B: one `llm`, a separate trace, as the page says.
3. **`genai/promptfoo-eval`:**
   - `promptfoo eval` with one prompt, one `contains` assertion and one `llm-rubric` assertion, against the mock's OpenAI routes.
   - Its OTLP export goes to the receiver.
   - Turn its telemetry and update checks off with the switches the spike names. If it insists on a network call, stop.
   - Expected: `llm` ×2, `evaluator` ×2, `span`.

**Done when:** as IN6.5, for these three cases.

**Commit:** `Capture n8n, Langflow and Promptfoo`

## IN6.8 — Pages

**Goal:** the pages listed in § What it is.

Captured pages follow the M5 captured-page shape (`litellm-proxy.mdx`):
- a `<Backing>` box;
- What you get, from `expected.json`;
- setup as built;
- Limits, from the spikes' findings:
  - n8n has no content;
  - Langflow's model call is a separate trace;
  - Prompt Flow needs the full `/v1/traces` URL;
  - LiveKit's voice spans are not captured, and `allow_pii` must stay at its default;
  - Pipecat has no tool span and no output on a tool-call turn;
  - OpenLIT: pin `>=1.37`, disable the HTTP instrumentors, and its `pricing_json`;
  - MCP: `mcp>=2` native, no content; the registry row's role;
  - the vector databases: which operations are retrievers;
  - Promptfoo: its own switches.

Setup-only pages follow M5's setup-only pages.

**Steps.**
1. **Setup-only pages:**
   - Stagehand: the v4 `telemetry` option. Its spans are OTLP/JSON, and no LLM span was seen.
   - LobeChat: unverified, `ENABLE_TELEMETRY=1`.
   - Gradio: points at OpenInference, OpenLLMetry or OpenLIT for the client library.
   - Ragas, UQLM, Azure AI Evaluation: trace the model calls with the provider's integration, then send each result with `cetra.score`. The sample code follows the existing scores docs; check it against `sdks/python/src/cetra`'s exports.
   - Cleanlab: the same; it needs a cloud key.
   - Harbor and Terminal-Bench: `harbor trace export --format otel --encoding pb --output traces/`, then POST each file as `application/x-protobuf` with the bearer header. Not run.
2. **Captured pages:** as above, after IN6.5 to IN6.7.
3. **Docs edits:**
   - `reference/attributes.md`: the new keys read (`span_type`, `trace_type`, `lk.pii.*`, `llm.usage.*` on scope `promptflow`, `gen_ai.operation.type`, and the rest);
   - `concepts/conventions.mdx`: one sentence per scope-gated family, as M5 did for Bifrost;
   - then `just docs-gen` (or `node scripts/skill-references.mjs`, as M5).

**Done when:** as IN5.4.

**Commits:**
- `Add setup-only app and tool pages`, which may land early;
- `Add app and tool pages`.

## IN6.9 — Index, sidebar, records

**Goal:** as IN5.5.

**Steps.**
1. **`index.mdx`:**
   - Add an **Apps and tools** section after Frameworks: rows, tiers, and cards for tested rows.
   - Add the matching sidebar group in `astro.config.mjs`.
   - Update `integrations.test.mjs`'s expected groups.
   - Read `src/pages/integrations.astro` to confirm the marketing page needs no other change.
2. **Design and `mvp-decisions.md`:**
   - D-IN-26, with Q1–Q3's answers;
   - the § M6 rows as built;
   - § Not supported gains the list in § What it is;
   - the appendix rows for M6 resolved.
3. **CHANGELOG:**
   - `Added`: the pages, the captured integrations and per-scenario capture environments;
   - `Changed`: the scope-gated rules, and `tts`/`stt` as spans.
4. **Roadmap:** the M6 plan link, status "M1–M6 implemented", decisions `D-IN-1`–`D-IN-26`.

**Done when:** as IN5.5.

**Commit:** `List the app and tool pages and record D-IN-26`

## After the last phase

`just check`, a Sonnet code review, a GLM agent for the fixes, then a push to PR #242.
