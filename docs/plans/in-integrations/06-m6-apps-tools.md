# Track IN, M6: apps, voice, browser agents, tools and evaluation — plan

**What M6 adds:**
- Captured cases for n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, MCP, OpenLIT, the vector databases through OpenLIT and OpenLLMetry, and Promptfoo.
- Pages for every item the spikes kept: captured, setup only, or merged.
- The normalizer rules those captures need. All are gated on the instrumentation scope (D-IN-5, D-IN-6) or on a span name that only that scope writes.
- The `mcp` row of the Python SDK's instrumentor registry.

**What the capture tool learns:**
- the harness becomes its own uv package, so scenario environments can share it;
- per-environment Python scenario roots (`tools/capture/envs/<env>/scenarios/`);
- a second loopback address for the mock (`mock_host`);
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
  - D-IN-2, D-IN-3, D-IN-5, D-IN-6, D-IN-7, D-IN-8, D-IN-15, D-IN-25;
  - the fixture-group rule (the "New fixture group names" bullet, line 84).
- `spikes/m6-apps.md` and `spikes/m6-tools-eval.md`: the runs, span dumps and proposed rules this plan cites by ID.
- `05-m5-gateways.md`: IN5.1, how tools and their environments are fetched; and Amendment B, the LiteLLM Proxy tool environment.
- `docs/spec/02-ingestion.md` § Mapping tables, § Cost computation; `docs/spec/10-sdk.md` § Instrumentors.
- `tools/capture/src/{run,scenario,scrub,receiver}.rs`, `tools/capture/fetch-tools.sh` and `tools/capture/py/src/capture_py/harness.py`.
- `crates/otel/src/{kind,context,usage,cost,payload}.rs`, `crates/otel/src/conventions/{mod,voltagent}.rs` and `crates/otel/src/convert/openai.rs`.

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
  - Stagehand, LobeChat, Gradio, Ragas, UQLM, Azure AI Evaluation, Cleanlab, and Harbor and Terminal-Bench, all setup only. Slugs and titles are in IN6.8.
- **Index:** an "Apps and tools" section.
- **SDK:** `instrument=["mcp"]` and the `cetra-sdk[mcp]` extra.

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
- **No cetra-core change.** No new `Convention` variant (D-IN-26 item 9): scoped content is read by a module keyed on the scope name.
- **Eval tools are setup only** unless Q4 is answered (b). Ragas, UQLM and Azure AI Evaluation show "trace the LLM calls, send results with `cetra.score`" (D-IN-26 item 5).

## Questions for the user (asked 2026-11-24, unanswered)

| # | Question | Blocks |
|---|---|---|
| Q1 | Vector-database `retriever` kind: (a) reads only (query, search, get, fetch), writes stay `span` — recommended; or (b) every operation | IN6.4b step 1, IN6.6 vector cases |
| Q2 | OpenLIT writes `gen_ai.usage.cost = 0` when its price table has no row: (a) a reported cost of exactly 0 on a span with tokens counts as no report, every source — recommended; (b) the same, OpenLIT scopes only; (c) no rule, the page documents `pricing_json` | IN6.4b step 2, IN6.6 OpenLIT cases, IN6.8 `tracing/cost-and-tokens.mdx` |
| Q3 | Morph Reflex, Testable Minds: URLs, or not supported | IN6.9 records only |
| Q4 | Ragas, UQLM, Azure AI Evaluation: (a) setup-only pages — recommended (their samples would add heavy packages to the SDK's dev group); (b) SDK-test pages, as the spike and design say | IN6.8's three pages only (`ragas`, `uqlm`, `azure-ai-evaluation`) |

Phases that do not depend on them run first. The answers are written here, then into D-IN-26. This plan covers Q4 (a). On (b), those three pages leave IN6.8, and a phase IN6.8b is written after the answer (Edit: `sdks/python/{pyproject.toml,uv.lock,tests/test_examples.py}` and the three pages).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-2 | Endpoint host gives the provider (`ENDPOINT_PROVIDERS`) |
| D-IN-3 | The harness routes a turn's real host to the mock |
| D-IN-5 | Scope-gated kind, provider and content rules |
| D-IN-6 | Plain `input`/`output` read only on a named scope (Pipecat) |
| D-IN-7 | Python registry rows: extra, `all`, `uv.lock`, spec 10, the SDK reference page |
| D-IN-8 | Tiers: captured, hand-written, SDK test, setup only |
| D-IN-15 | Framework names skipped as providers |
| D-IN-25 | Tool binaries and tool environments under `target/capture-tools`, checksummed or locked |
| D109 | Reported cost wins outright (amended by Q2 (a) or (b)) |
| D-IN-26 (new, recorded by IN6.9) | Items below |

D-IN-26, M6's decisions:

1. **The harness is a package; Python scenarios may have their own environment root.**
   - `tools/capture/harness/` is a uv package `cetra-capture-harness` (module `capture_py`, with `py.typed`). It holds what was `tools/capture/py/src/capture_py/`.
   - `tools/capture/py` and every environment depend on it by path (editable). Scenario imports (`from capture_py.harness import …`) do not change.
   - A Python scenario that needs its own environment lives under `tools/capture/envs/<env>/scenarios/<group>/<case>/`. There is no `project` field in `scenario.json`.
   - `discover()` also scans `tools/capture/envs/*/scenarios` and returns the environment name. The program is `uv run --project tools/capture/envs/<env> --frozen python main.py`. A `group/case` path is unique across all roots.
   - Each environment is a uv project with its own `uv.lock`:
     - `package = false`;
     - `exclude-newer = "7 days"`;
     - `cetra-capture-harness = { path = "../../harness", editable = true }` in `[tool.uv.sources]` and in `dependencies`;
     - the capture project's `[tool.ruff]` (with `known-first-party = ["capture_py"]`) and `[tool.mypy]`, and a `dev` group pinning `ruff` and `mypy` at `tools/capture/py`'s versions.
   - The lint trio runs in `tools/capture/harness`, in `tools/capture/py` and in each environment.
   - An environment is used when a case's packages cannot share the capture project's lock (spike findings: OpenLIT needs `openai<3`, mcp 2 conflicts with semantic-kernel).
   - IN6.5 made two: `envs/promptflow` (promptflow-tracing needs opentelemetry-sdk<1.39, so that environment overrides the OpenTelemetry set to 1.38.0 and pins azure-monitor-opentelemetry-exporter to 1.0.0b45, the newest release whose OpenTelemetry requirement takes 1.38 and which promptflow-devkit's OTLP path imports) and `envs/livekit` (livekit-agents needs json-repair 0.63, beeai-framework needs below 0.61).
2. **Tool environments** (servers and CLIs the scenario launches; M5's pattern) are fetched by `just capture-tools` into `target/capture-tools/<name>/<version>/`:
   - **n8n and Promptfoo:** npm packages from `tools/capture/n8n/` and `tools/capture/promptfoo/`. Each holds a `package.json`, a `pnpm-workspace.yaml` (the shape of `tools/capture/ts`'s: `packages: []`, `minimumReleaseAge`, `allowBuilds`) and a `pnpm-lock.yaml`. They install with `pnpm install --frozen-lockfile --dir <project> --modules-dir <target dir>/node_modules`.
   - **Langflow:** a uv project `tools/capture/langflow/`, like `litellm-proxy`.
   - **Python environments** (`tools/capture/envs/*`) are synced by `uv sync --frozen --project` and skipped when a copy of their `uv.lock` in `target/capture-tools/envs/<name>.lock` matches.
   - **Browser Use** needs Chromium: an explicit `playwright==1.65.2` pin in `tools/capture/envs/browser-use` installs it (item 10; version locked, not checksummed).
3. **New fixture groups:** `n8n`, `livekit` and `pipecat`, which the design already lists, plus `promptflow` and `langflow`, which amend the design's fixed list (IN6.9 edits that bullet, `00-design.md` line 84). Browser Use, MCP, OpenLIT and Promptfoo go in `genai`; the OpenLLMetry vector case goes in `openllmetry`. `website/scripts/check-backing.mjs` accepts `[a-z0-9_]+` groups (`n8n` has a digit).
4. **Pages with no capture:**
   - setup only: Stagehand, LobeChat, Weaviate (a section of the Vector databases page), Pinecone (the same page), Cleanlab, Harbor and Terminal-Bench;
   - merged: Gradio, a setup-only page pointing at the instrumentors of the client library.
5. **Eval tools.** Ragas, UQLM and Azure AI Evaluation get setup-only pages, not SDK-test pages (Q4 (a)). Their samples are not run.
6. **OTLP/JSON enum names stay rejected.** The Harbor page uses `harbor trace export --encoding pb` and a POST of each file.
7. **The `mcp` registry row is added** (OpenInference's `MCPInstrumentor`; the registry has no such row today). It joins an `mcp<2` server's spans to the client trace; the page says it records no spans of its own, and that `mcp>=2` traces natively.
8. **n8n names are not rewritten.** The stored span name stays `node.execute` / `workflow.execute` (the spike's R-N8N-1 is dropped: no rule renames a span, and a rename would be a new mechanism). The page's Limits says the node name is in `n8n.node.name`.
9. **Scoped content has no convention.** A fixed scope table (`pipecat*`, `livekit-agents`, `promptflow`, `langflow`) in `crates/otel/src/conventions/scoped.rs` fills input and output only where the convention readers left them empty.
10. **Chromium** for Browser Use comes from an explicit `playwright==1.65.2` pin in `tools/capture/envs/browser-use` (the newest upload before the cutoff; browser-use 1.0.6 does not lock playwright). `playwright install chromium` installs it. The version is locked by `uv.lock`, not checksummed. The scenario passes the installed binary as `executable_path`.
11. **Not read, and unit-tested only:**
    - Promptfoo's `promptfoo.request.body` and `promptfoo.response.body` are not read (the grader span is `evaluator`, its child `chat` span keeps tokens, and has no content);
    - R-OL-1 and R-OL-3 are unit-tested only: no captured case has an OpenLIT Browser Use 0.x span (Browser Use 1.0 is captured);
    - the Pinecone and Weaviate span-name rows are unit-tested only: no capture runs them.
12. **Rules.** Items 13 to 15 are recorded with Q1–Q3's answers.

## Versions

Every version is the newest upload on or before the 7-day cutoff of the day the phase runs. The spike's values are at the cutoff of 2026-11-17:

| Package | Spike version | Where |
|---|---|---|
| `n8n` (npm) | 2.48.4 | `tools/capture/n8n/` |
| `promptfoo` (npm) | 0.125.1 | `tools/capture/promptfoo/` |
| `langflow` | 1.13.2 | `tools/capture/langflow/` |
| `promptflow-tracing`, `promptflow-devkit` | 1.18.5 | `envs/promptflow` (OpenTelemetry 1.38.0 overrides; `azure-monitor-opentelemetry-exporter` 1.0.0b45, D-IN-26 item 1) |
| `livekit-agents`, `livekit-plugins-openai` | 1.8.12 | `envs/livekit` (the capture project's lock refuses it: json-repair, D-IN-26 item 1) |
| `pipecat-ai[openai]` | 1.16.0 | the same |
| `browser-use` | 1.0.6 | `envs/browser-use` |
| `playwright` | 1.65.2, an explicit pin (newest upload before the cutoff, 2026-11-14; browser-use 1.0.6 does not lock it); locked by `uv.lock`, not checksummed | `envs/browser-use` (Chromium, item 10) |
| `mcp` | 2.3.0 | `envs/mcp` |
| `openlit` | 1.45.0, with `openai<3` | `envs/openlit` |
| `chromadb`, `qdrant-client`, `pymilvus[milvus_lite]` | 1.5.9, 1.20.0, 3.0.3 | `envs/openlit` and `envs/vector` |
| `opentelemetry-instrumentation-{chromadb,qdrant}` | 0.63.1 | `envs/vector` |
| `opentelemetry-instrumentation-milvus` | 0.60.0, because 0.63.1 cannot install (spike finding 3) | `envs/vector` |
| `openinference-instrumentation-mcp` | 2.1.5 (2.1.8 is 2026-11-21, too new) | `sdks/python` extra `mcp` |
| harness dependencies | `httpx==0.28.1`, `httpx2==2.16.0`, `opentelemetry-sdk==1.45.2`, `opentelemetry-exporter-otlp-proto-http==1.45.2`: the versions `tools/capture/py/pyproject.toml` pins today | `tools/capture/harness/` |

The implementer:
- runs `uv lock`, `npm view` or `pnpm` resolution on the day;
- records the versions it used;
- stops on any version that differs from the spike's in its major or minor number, or that will not install.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN6.0 Harness package | `tools/capture/harness/` (moved files, new project files), `tools/capture/py/{pyproject.toml,uv.lock}`, `justfile`, `tools/packaging/release-prep.sh`, `tools/README.md` | IN6.S (so the capture locks are made after the extra exists) |
| IN6.1 Capture tool | `tools/capture/src/{scenario,run,scrub,receiver,write,error,lib,main}.rs`, `tools/capture/fetch-tools.sh`, `justfile`, `docs/plans/b2-capture/README.md`, new `tools/capture/{n8n,promptfoo,langflow}/` and `tools/capture/envs/{browser-use,mcp,openlit,vector}/` (project and lock files only), `crates/otel/tests/support/mod.rs`, `fixtures/otlp/README.md`, `website/scripts/check-backing.{mjs,test.mjs}`, `tools/packaging/release-prep.sh`, `tools/README.md`, `AGENTS.md` | IN6.0 |
| IN6.2 Voice rules | `crates/otel/src/{kind,context,payload,usage,normalize}.rs`, `crates/otel/src/conventions/{mod,scoped}.rs`, `docs/spec/02-ingestion.md` | — |
| IN6.3 App rules | `crates/otel/src/{kind,context,usage}.rs`, `crates/otel/src/conventions/scoped.rs`, `docs/spec/02-ingestion.md` | IN6.2 (same files) |
| IN6.4a OpenLIT, Browser Use and Promptfoo rules | `crates/otel/src/{kind,context}.rs`, `docs/spec/02-ingestion.md` | IN6.3 (same files) |
| IN6.4b Vector and zero-cost rules | `crates/otel/src/{kind,cost,normalize}.rs`, `docs/spec/02-ingestion.md`, `docs/mvp-decisions.md` | IN6.4a; Q1, Q2 |
| IN6.S SDK registry row `mcp` (runs first) | `sdks/python/{pyproject.toml,uv.lock,README.md,src/cetra/_instrument.py,tests/test_instrument.py}`, `tools/capture/py/uv.lock`, `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/index.mdx`, `skills/cetra/references/python.md` (generated) | — |
| IN6.5 Captures A | scenarios + fixtures: promptflow, livekit, pipecat, browser-use, mcp; `tools/capture/harness/src/capture_py/harness.py`; `tools/capture/py/{pyproject.toml,uv.lock}` (or a new `envs/<name>/`) | IN6.1, IN6.3 (includes IN6.2) |
| IN6.6 Captures B | scenarios + fixtures: openlit-chat, openlit-vector, openllmetry vector | IN6.1, IN6.4b |
| IN6.7 Captures C | scenarios + fixtures: n8n, langflow, promptfoo | IN6.1, IN6.4a (includes IN6.3) |
| IN6.8 Pages | `website/src/content/docs/docs/integrations/*.mdx`, `reference/attributes.md`, `concepts/conventions.mdx`, `tracing/cost-and-tokens.mdx`, `skills/cetra/references/*.md` (generated) | IN6.5–IN6.7 for the captured pages (MCP also IN6.S); the setup-only pages depend on nothing |
| IN6.9 Index, sidebar, records | as IN5.5 | IN6.8, IN6.S |

**Parallel runs:**
- IN6.S first: it changes `cetra-sdk`'s extras, which every capture lock embeds, so the capture locks are made after it. Then IN6.0, which moves files; IN6.1 starts after its commit.
- IN6.2 has files disjoint from IN6.S, IN6.0 and IN6.1 and can run beside any of them.
- IN6.2 → IN6.3 → IN6.4a → IN6.4b run in order: they edit the same files.
- IN6.5, IN6.6 and IN6.7 have disjoint files (no shared-file edits between capture phases: only IN6.5 edits the harness and `tools/capture/py`'s project files; IN6.6 and IN6.7 add new directories only). They may run at the same time; machine load rules apply (run the captures one at a time).
- The setup-only half of IN6.8 can run beside any phase.
- IN6.5 needs IN6.3 only for blessing. It may capture to the scratchpad first, as M5's IN5.3 did.

---

## IN6.S — SDK registry row `mcp`

**Runs first** (before IN6.0).

**Goal:** `instrument=["mcp"]` and the `cetra-sdk[mcp]` extra, as D-IN-7 lists (the registry has no `mcp` row today). Mirror how the M3 rows were added (commit `26f6243`: `git show 26f6243` shows every file).

**Steps.**
1. **Verify before writing:** in `openinference-instrumentation-mcp` at the newest version on or before the 7-day cutoff (2.1.5 at 2026-11-17), the module is `openinference.instrumentation.mcp` and the class `MCPInstrumentor`, and read its `requires-python`. If either name differs, stop. If it excludes a Python version the SDK supports (≥ 3.11), add the marker as `instructor` and `beeai` do (`; python_version < "…"`), and say so in spec 10's table.
2. `sdks/python/src/cetra/_instrument.py`: a row in `REGISTRY`: `Instrumentor(name="mcp", extra="mcp", module="openinference.instrumentation.mcp", cls="MCPInstrumentor")`.
3. `sdks/python/pyproject.toml`: the extra `mcp = ["openinference-instrumentation-mcp>=<version>"]` and the same requirement in `all`. The dev group does not grow (D-IN-21). Run `uv lock` in `sdks/python`.
4. `sdks/python/tests/test_instrument.py`: the docstring of `test_other_registry_entries_import` counts the names ("The nineteen other names": now twenty). The parametrized test covers the row; it skips when the extra is not installed.
5. Docs: `docs/spec/10-sdk.md` (the `## Python API` heading's extras list, the `### Instrumentors` table row, and one sentence: the row records no spans of its own, it joins an `mcp<2` server's spans to the client trace, and `mcp>=2` traces natively); `sdks/python/README.md` and `website/src/content/docs/docs/reference/python/index.mdx` (the extras table); then `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` for `skills/cetra/references/python.md`.

**Tests:** the parametrized import test (step 4). With the extra installed, `resolve(["mcp"])` returns the class.

6. `tools/capture/py/uv.lock` embeds `cetra-sdk`'s metadata (the only capture lock that exists at this point): run `uv lock` in `tools/capture/py` and commit the result.

**Done when:** `just check-py`; `cd website && node scripts/skill-references.mjs`; `cd tools/capture/py && uv lock --check`.

**Commit:** `Add MCP to the instrumentor registry`

## IN6.0 — Harness package

**Goal:** the capture harness is its own uv package, `tools/capture/py` depends on it, and every capture still passes.

**Edit:**
- `tools/capture/harness/{pyproject.toml,uv.lock,.python-version}` (new);
- `tools/capture/harness/src/capture_py/{__init__.py,harness.py,py.typed}` (`git mv` of the first two from `tools/capture/py/src/capture_py/`; `py.typed` is new and empty);
- `tools/capture/py/{pyproject.toml,uv.lock}`;
- `justfile`;
- `tools/packaging/release-prep.sh`;
- `tools/README.md`.

**Steps.**
1. `git mv tools/capture/py/src/capture_py tools/capture/harness/src/capture_py`, then delete the empty `tools/capture/py/src`. Add the empty `py.typed`. Copy `.python-version`.
2. `tools/capture/harness/pyproject.toml`:
   - `[project]` name `cetra-capture-harness`, version `0.0.0`, `requires-python = "==3.13.*"`.
   - `dependencies`, exactly what `harness.py` imports (`httpx`, `httpx2`, `cetra`, `opentelemetry`, the OTLP/HTTP exporter, the SDK; the exporter's `_log_exporter` ships in the exporter package): `httpx==0.28.1`, `httpx2==2.16.0`, `opentelemetry-sdk==1.45.2`, `opentelemetry-exporter-otlp-proto-http==1.45.2`, `cetra-sdk`.
   - `[tool.uv.sources]`: `cetra-sdk = { path = "../../../sdks/python", editable = true }`.
   - `[tool.uv]`: `exclude-newer = "7 days"`.
   - `[build-system]` and `[tool.uv.build-backend]`: copied from `tools/capture/py` (`uv_build`, `module-name = "capture_py"`).
   - `[dependency-groups]` `dev = ["ruff==0.17.3", "mypy==2.4.1"]`.
   - `[tool.ruff]`, `[tool.ruff.lint]`: copied from `tools/capture/py`, with `known-first-party = ["capture_py"]` under `[tool.ruff.lint.isort]`.
   - `[tool.mypy]`: `strict = true`, `python_version = "3.13"`, `files = ["src"]`.
   - Run `uv lock` in the directory.
3. `tools/capture/py/pyproject.toml`:
   - add `"cetra-capture-harness"` to `dependencies` and `cetra-capture-harness = { path = "../harness", editable = true }` to `[tool.uv.sources]`;
   - keep every other pin, including `httpx`, `httpx2`, `cetra-sdk` and the OTel pins (scenarios import them; the versions are the harness's);
   - remove `[build-system]` and `[tool.uv.build-backend]`; add `package = false` to `[tool.uv]`;
   - `[tool.mypy]`: `files = ["scenarios"]`, remove `mypy_path`;
   - `[tool.ruff.lint.isort]`: `known-first-party = ["capture_py"]`, so import order does not change now that `src/` is gone;
   - run `uv lock`; report every pin the lock moved (there should be none beyond the root package).
4. `justfile`:
   - new recipe `capture-harness-sync`: `cd tools/capture/harness && uv sync --locked`;
   - `check-capture` depends on `capture-harness-sync` too, and runs the lint trio in `tools/capture/harness` before the one in `tools/capture/py`.
5. `tools/packaging/release-prep.sh` line 64 (`(cd tools/capture/py && uv lock)`): the harness and the project both lock `cetra-sdk`'s version. Replace it with a loop over `tools/capture/harness`, `tools/capture/py` and `tools/capture/envs/*/`, locking each directory that has a `uv.lock` (`[ -f "$dir/uv.lock" ] || continue`).
6. `tools/README.md`: one paragraph: the harness is `tools/capture/harness`, imported as `capture_py`.
7. **References to the old path.** Everything else was checked and needs no change: no `.github` workflow mentions `capture_py` or `tools/capture/py`; the TypeScript project does not reference the Python one; `tools/capture/src/run.rs` (`python()`, and its tests) names the project directory `tools/capture/py`, which stays. The `capture_py` hits are `tools/capture/py/pyproject.toml` (`module-name`, moved) and the scenarios' imports (unchanged). Search again (`grep -rn "capture_py\|capture/py/src"` outside `target`, `.venv` and `docs/plans`) and fix anything new.

**Tests:** none to add. The move is checked by the commands below.

**Done when:**
- `cd tools/capture/harness && uv lock --check && uv run ruff format --check && uv run ruff check && uv run mypy`;
- `cd tools/capture/py && uv lock --check && uv run ruff format --check && uv run ruff check && uv run mypy`;
- `just capture-tools` first (the `litellm-proxy` case needs its tool environment), then `cargo run -q -p cetra-capture -- run genai/openai-v2-chat-tool genai/litellm-proxy-chat --check` (an in-process scenario and a tool scenario) passes with no fixture change;
- `git status` shows no change under `fixtures/`.

**Commit:** `Move the capture harness into its own package`

## IN6.1 — Capture tool

**Goal:** Python scenarios can live under their own environment root; the mock can bind a second loopback address; tool environments are fetched; the new groups exist.

**Steps.**
1. **`scenario.rs`:**
   - `GROUPS`: add `n8n`, `livekit`, `pipecat`, `promptflow` and `langflow`.
   - `Scenario` gains `#[serde(default = "default_mock_host")] pub mock_host: String`, default `"127.0.0.1"`. Validation (in `check`): the value parses as `std::net::Ipv4Addr` and `is_loopback()`.
   - Update the module doc and `docs/plans/b2-capture/README.md` § The scenario format with `mock_host`.
   - `write.rs`'s test helper `scenario()` builds a `Scenario { … }` literal (line ~186): it gets `mock_host: "127.0.0.1".to_owned()`. No other `Scenario { … }` literal exists (`run.rs` and `scenario.rs` tests build from JSON, where the default applies).
   - Tests: the default; `127.0.0.2` accepted; `10.0.0.1`, `localhost` and `::1` rejected; `full_scenario_round_trips` carries a non-default value.
2. **`run.rs`, discovery:**
   - `discover` returns `Vec<Discovered>` instead of 3-tuples. `pub struct Discovered { pub case: CasePath, pub language: Language, pub env: Option<String>, pub dir: PathBuf }`, exported from `lib.rs`. Update the callers: `main.rs` (`list`, `run`, `select`) and the `discover` tests in `run.rs`.
   - `scan` is called for `tools/capture/py` and `tools/capture/ts` as today (`env: None`), and for every `tools/capture/envs/<env>/scenarios` (`Language::Python`, `env: Some(<env>)`). `scan` takes the scenarios directory and the env.
   - An `envs/<env>` directory that has `scenarios/` but no `pyproject.toml` is `CaptureError::ScenarioInvalid` (case: the directory path).
   - Duplicate check: the same `group/case` in any two roots fails with `CaptureError::DuplicateCase`. Change that variant's message in `error.rs` to name the roots generally ("case {case} is defined in more than one scenario root").
   - Tests: a case under `envs/x/scenarios` is found with `env == Some("x")`; the same case in `py` and in an env fails; the same case in two envs fails; an env without `pyproject.toml` fails; the result stays sorted by group, then case.
3. **`run.rs`, program and mock:**
   - `Program::for_language(repo_root, language, env: Option<&str>)`; with `Some(env)` Python uses `tools/capture/envs/<env>`, otherwise `tools/capture/py`. `main.rs` passes `found.env.as_deref()`. Update the program tests.
   - `bind(router)` takes the host: the mock binds `scenario.mock_host`, the receiver `127.0.0.1`.
   - `program_env` builds `CETRA_CAPTURE_MOCK_URL` as `http://<scenario.mock_host>:<port>`; the receiver URLs keep `127.0.0.1`.
   - `process` and `ScrubContext` gain `mock_host: String`.
   - Tests: `program_env` with the default host and with `127.0.0.2`.
4. **`scrub.rs`** (the host the scenario chose must not leak either):
   - `replacements`: add `http://<mock_host>:<mock_port>` → `https://<provider host>`.
   - `rewrite_address`: also rewrites a `server.address` equal to `mock_host`.
   - `leak_needles`: add `mock_host` and `<mock_host>:<mock_port>`.
   - Tests (the file's `ctx()` gets `mock_host: "127.0.0.1"`): a request with `http://127.0.0.2:<port>` and `server.address = 127.0.0.2` is rewritten; a leftover `127.0.0.2` is a leak.
5. **`receiver.rs`, chunked bodies:**
   - Add the test `a_chunked_otlp_body_is_received`: a POST with `Transfer-Encoding: chunked` and no `Content-Length` carries a protobuf `ExportTraceServiceRequest`, and the spans are recorded.
   - If it fails, fix the receiver. If the fix is not local to `receiver.rs`, stop and report.
6. **Group lists:** add the five groups to `crates/otel/tests/support/mod.rs` `GROUPS` and to `fixtures/otlp/README.md` § Layout (the sentence listing group directories).
   - `website/scripts/check-backing.mjs` line 23: the fixture regex `^[a-z_]+\/…` becomes `^[a-z0-9_]+\/[a-z0-9-]+$` (`n8n` has a digit). Add a test in `check-backing.test.mjs` with a `n8n/agent-webhook` fixture that exists, and one that does not.
7. **Environments** (project and lock files only, no code). Each Python environment depends on `cetra-capture-harness` as in D-IN-26 item 1 and carries the sections it lists. The harness already carries `httpx`, `httpx2` and the `opentelemetry-*` pins; an environment lists a package again only when its tool needs another version. Report each cap.

   | Directory | Content |
   |---|---|
   | `tools/capture/envs/browser-use/` | `browser-use`, `pydantic-ai` as `browser-use` pins it |
   | `tools/capture/envs/mcp/` | `mcp==2.3.0` |
   | `tools/capture/envs/openlit/` | `openlit==1.45.0`, `openai<3` (the newest 2.x at the cutoff), `chromadb`, `qdrant-client`, `pymilvus[milvus_lite]` at § Versions |
   | `tools/capture/envs/vector/` | the three vector clients, `opentelemetry-instrumentation-{chromadb,qdrant}==0.63.1`, `opentelemetry-instrumentation-milvus==0.60.0` |
   | `tools/capture/langflow/` | `langflow`, a tool environment like `tools/capture/litellm-proxy` (`package = false`, no harness) |
   | `tools/capture/n8n/`, `tools/capture/promptfoo/` | `package.json` (exact version, `"private": true`, the `packageManager` of `tools/capture/ts/package.json`), `pnpm-workspace.yaml`, `pnpm-lock.yaml` |

   - Each pnpm `pnpm-workspace.yaml` has the shape of `tools/capture/ts/pnpm-workspace.yaml`: `packages: []`, `minimumReleaseAge: 10080`, and an `allowBuilds` map. The implementer lists every package of the lock that has a build script, sets each `true` or `false` with a one-line reason (as `tools/capture/ts` does), and reports the list. **Stop** if an install fails on a build script.
   - Each Python environment also has a `.python-version` (copy the project's).
   - Each environment's `[tool.mypy]` gets a `[[tool.mypy.overrides]]` with `ignore_missing_imports = true` for each of its packages that ships no types, as `tools/capture/py` does for `smolagents` and `guardrails`. The implementer lists the packages in the report.
8. **`fetch-tools.sh`:** after the LiteLLM Proxy environment, install, each skipped when its lock copy matches (M5's rule):
   - Langflow, into `target/capture-tools/langflow/<version>`, as LiteLLM;
   - n8n and Promptfoo: `pnpm install --frozen-lockfile --dir tools/capture/<name> --modules-dir target/capture-tools/<name>/<version>/node_modules`, lock copy `target/capture-tools/<name>/pnpm-lock.yaml`;
   - every `tools/capture/envs/*` environment: `uv sync --frozen --project tools/capture/envs/<name>`, which writes its `.venv` in place; skipped when `target/capture-tools/envs/<name>.lock` equals the environment's `uv.lock` and its `.venv` exists;
   - Chromium, for Browser Use: `PLAYWRIGHT_BROWSERS_PATH=target/capture-tools/playwright uv run --project tools/capture/envs/browser-use --frozen playwright install chromium`, skipped when `target/capture-tools/playwright` holds a `chromium-*` directory. Before writing this, check that the `browser-use` lock holds `playwright`; if not, **stop**.
9. **`justfile`, `check-capture`:** after the `tools/capture/py` trio, one loop runs the trio in each `tools/capture/envs/*` directory (`for env in tools/capture/envs/*/; do [ -d "${env}scenarios" ] || continue; (cd "$env" && uv run --locked ruff format --check && uv run --locked ruff check && uv run --locked mypy) || exit 1; done`). The loop skips an environment without a `scenarios/` directory (mypy has nothing to check there). The scenarios of an environment are under that environment, so `tools/capture/py`'s mypy does not see them. Nothing else in the `justfile` changes: the other recipes need no new environment, since `uv run` syncs an environment on first use.
10. **Docs and records:**
    - `AGENTS.md`: the Tests line at 97 gains "A capture scenario whose packages cannot share `tools/capture/py`'s lock lives under `tools/capture/envs/<env>/scenarios/` (D-IN-26)."; the Layout row at 131 and the `just capture-tools` row at 208 name the harness, the environments and the tool projects; the Required tools line at 232 adds the new environments.
    - `tools/README.md`: scenarios may live under `tools/capture/envs/<env>/scenarios/`.
    - `tools/packaging/release-prep.sh` already loops over `envs/*` (IN6.0); check it again.

**Done when:**
- `cargo nextest run -p cetra-capture`, plus clippy and fmt for it;
- `cargo nextest run -p cetra-otel --test fixtures`, for the groups;
- `just capture-tools`, twice; the second run installs nothing;
- the lint trio in `tools/capture/harness` and in `tools/capture/py`; the environments have no `scenarios/` directory yet, so the loop skips them (it runs from IN6.5 on);
- `node --test website/scripts/`;
- `bash -n tools/capture/fetch-tools.sh`.

**Commit:** `Let capture scenarios use their own environments`

## IN6.2 — Voice rules (Pipecat, LiveKit)

**Goal:** rules R-PC-1 to R-PC-4 and R-LK-1 to R-LK-3 of `spikes/m6-apps.md`, as amended here, each with unit tests. Spec 02 changes in the same commit.

**Steps.** For each rule, read the spike's dump in § 4 (LiveKit) or § 5 (Pipecat) and reuse the existing mechanism named below.

1. **Span name reaches the kind table.** `kind.rs` `span_kind(bag, span_name, scope_name, conventions)` gains `span_name: &str` after `bag`. Update the caller (`normalize.rs`, the `span_kind(&mut bag, scope, &conventions)` call: pass `&span.name`) and every call in `kind.rs`'s tests (the helper near line 344 and the direct calls). `scope_gated(bag, scope_name)` gains `span_name` the same way. Name-keyed rows use it; no existing test changes its expectation.
2. **R-PC-2** (global kind table, `genai_operation`): `tts` and `stt` give `SpanKind::Span`. No other realtime value is added.
   - Test: `gen_ai.operation.name = tts` with a `gen_ai.request.model` is `span` (no `llm` fallback), and the operation column keeps `tts`.
3. **R-LK-2** (scope `livekit-agents`; M2): a new function `livekit(span_name, scope_name) -> Option<SpanKind>`, called in `span_kind` right after the `cetra.span.kind` row and **before** the `gen_ai.operation.name` row:
   - `llm_node` → `chain`;
   - `llm_request_run` and `start_agent_activity` → `span`;
   - it consumes nothing.
   - `llm_request` stays `llm` (its `gen_ai.operation.name = chat` decides).
   - Tests: `start_agent_activity` with `gen_ai.operation.name = create_agent` is `span` on `livekit-agents` and `agent` on another scope; `llm_node` with a request model is `chain`, and `llm` on another scope; `llm_request_run` is `span`.
4. **R-PC-3, kind** (scope `pipecat.turn`): in `scope_gated`, span names `conversation` and `turn` give `chain`. Test: both are `chain` on `pipecat.turn`, `span` on `pipecat`.
5. **R-PC-3, session:** a new function `pipecat_session(bag, scope_name, session)` in `context.rs`, called in `fill` right after `voltagent_session`, written like it: on scope `pipecat.turn`, when the session is still empty, a non-empty string `conversation.id` fills it and is consumed.
   - Tests: it fills on scope `pipecat.turn`; it is not read on scope `pipecat` or on another scope; `gen_ai.conversation.id` still wins when present.
6. **R-LK-1** (scope `livekit-agents`; M3): `context.rs` `provider(bag, resource)` gains `scope_name` (`fill` passes it). On scope `livekit-agents`, when `gen_ai.provider.name` holds a string containing `.` or `:`:
   - the key is taken (consumed, as `first_string` does today);
   - the string goes through `host_of`, then `match_endpoint`;
   - a mapped host gives that provider; an unmapped host gives an empty provider, and no later source of `PROVIDER` is read;
   - any span kind (the provider column is filled for every kind);
   - elsewhere nothing changes.
   - No row is added to `ENDPOINT_PROVIDERS`: `match_endpoint("api.openai.com")` stays `None` (the assertion at `context.rs:1172`) and D-IN-2 is unchanged. Inside this branch, a host equal to `api.openai.com` gives `openai`; any other host goes through `match_endpoint`.
   - Spec 02: one sentence under § Model, provider, usage for the LiveKit rule (host `api.openai.com` gives `openai`, other mapped hosts their provider, an unmapped host an empty provider). The § Provider from the endpoint table does not change.
   - Tests: `api.openai.com` gives `openai`; `127.0.0.1:1234` gives an empty provider and the key is consumed; a mapped host such as `openrouter.ai` gives `openrouter`; a plain `openai` value is unchanged; the same host string on another scope stays the provider string (as today); `match_endpoint("api.openai.com")` is still `None`.
7. **Scoped content** (R-PC-1, R-LK-3; M6). New module `crates/otel/src/conventions/scoped.rs` (`mod scoped;` in `conventions/mod.rs`) with one function `pub(crate) fn read(ctx: &mut ReadContext<'_, '_>, filled: &Io) -> Io`.
   - `payload::assemble` calls it after the loop over `readers` and before the `drop_lower_payloads` loop, for every span. It sets `io.input` and `io.output` from the result only where they are still `None`. It adds no convention to `winners`.
   - `read` acts only on a fixed scope table and returns only the fields `filled` lacks. A key is consumed (`bag.take_payload`) only when it fills its field. An empty string is absent (as `voltagent.rs`'s `plain`).
   - Value rule, for every key: `non_message_payload(value, ctx.warnings)` (rule 10); for OpenAI-shaped arrays see below. A failed conversion stores the plain value and adds no `errors` entry.
   - **Pipecat** (scope starts with `pipecat`): `input` and `output`.
     - `input`: `decode_json_text`; when it is an array and `crate::convert::openai_messages(&json)` is `Ok`, the payload is `Payload::Messages`; otherwise `non_message_payload`.
     - `output`: `non_message_payload`.
   - **LiveKit** (scope `livekit-agents`), keyed by attribute (each input key is written by one span only, spike § 4): input from `lk.pii.user_input` (`agent_turn`), else `lk.pii.chat_ctx` (`llm_node`; LiveKit's own JSON, stored as JSON), else `lk.pii.function_tool.arguments`; output from `lk.pii.response.text` (written by both `agent_turn` and `llm_node`), else `lk.pii.function_tool.output`. The read order is the order written here. Because `read` fills only empty fields, the GenAI keys (`gen_ai.tool.call.arguments`, `.result`) win on `function_tool`. `lk.pii.input_text` and `lk.pii.user_transcript` are **not** read: no span in text mode writes them.
   - Add the scope table's entries for `promptflow` and `langflow` in IN6.3, not here.
   - Spec 02 `### Input / output`: one row for the scoped keys.
   - Tests:
     - Pipecat: an OpenAI message array `input` gives messages; a non-array, a non-OpenAI array and invalid JSON give the plain value with no `cetra.normalize_error`; text `output`; plain `input`/`output` on another scope stay unread; a Pipecat span whose convention reader already filled `input` keeps it and leaves `input` unconsumed.
     - LiveKit: each key fills its field; `function_tool` keeps `gen_ai.tool.call.*` over `lk.pii.function_tool.*`; the same keys on another scope stay unread.
8. **R-PC-4 (test only).** `gen_ai.request.top_k = "NOT_GIVEN"` goes into `request_params` as the string `"NOT_GIVEN"` (`request_params`, `insert_param`: the key is consumed, no warning). The spike said it stays in `attributes`; the code shows otherwise. No rule. Test: `fill` on a span with that key gives no warning, and `request_params` is `{"top_k":"NOT_GIVEN"}`.
9. **TTFT (test only).** `usage.rs` `ttft` already reads `gen_ai.response.time_to_first_chunk` (seconds) for every scope, so LiveKit needs no rule. Test: a `llm_request`-shaped span with that key and `langfuse.observation.completion_start_time` gets the GenAI value (nanoseconds).

**Tests:** as listed in each step, plus: the same attributes on another scope stay unchanged.

**Done when:** `cargo nextest run -p cetra-otel`, clippy and fmt pass, with no `expected.json` changed (stop if one changes).

**Commit:** `Read Pipecat and LiveKit spans`

## IN6.3 — App rules (Prompt Flow, Langflow, n8n)

**Goal:** rules R-PF-1 to R-PF-3, R-LF-1 to R-LF-2, R-N8N-2 and R-N8N-3 of `spikes/m6-apps.md` §§ 1–3. R-N8N-1 (the display name) is dropped (D-IN-26 item 8). Spec 02 changes in the same commit. Same method as IN6.2.

**Steps.**
1. **Prompt Flow** (scope `promptflow`, exact):
   - **Kind**, in `scope_gated`: `decide(bag, "span_type", …)` with:
     - `LLM` → `llm`;
     - `Embedding` → `embedding`;
     - `Retrieval` → `retriever`;
     - `Flow` and `LangChain` → `chain`;
     - `Function` → `span`.
   - **Models**, in `context.rs`: a new function `promptflow_models(bag, scope_name, request, response, warnings)` called in `fill` after `voltagent_model`. On scope `promptflow`, only on a span whose `span_type` is `LLM` or `Embedding` (peeked with `bag.get`; the key is consumed later by the kind row), an empty response model is filled from `llm.response.model` (consumed), and an empty request model from the `model` field of the `inputs` JSON (read with `decode_json_text`; `inputs` is not consumed here, it is the span's input).
   - **Tokens**, in `usage.rs`: `voltagent_usage` and its struct `VoltagentUsage` become `scoped_usage` and `ScopedUsage`; their gate is `kind == Llm && (conventions contains Voltagent || scope_name == "promptflow")`. `usage()`'s unused `_scope_name` parameter becomes `scope_name` and is passed in. Update the doc comments. `llm.usage.total_tokens` is already a source of `TOTAL`.
   - **IO**, in `scoped.rs` (add the scope to the table): input from `inputs`, output from `output`.
     - On an `llm` span, `inputs` holds the OpenAI request parameters. When it is an object with a `messages` array and `openai_messages(&inputs["messages"])` (the array itself) is `Ok`, the input is `Payload::Messages`; otherwise the plain value.
     - `output` is the OpenAI response object. When `choices[0].message` exists and `openai_messages` accepts `[that message]`, the output is `Payload::Messages`; otherwise the plain value.
     - Before writing this, check what the capture's dump holds (IN6.5 captures it; until then the spike § 3 only describes it). If it differs, stop.
   - **Detection.** `detect.rs` marks the `llm.` prefix as OpenInference. Write a test with both conventions on one span (`input.value` and `inputs`): OpenInference's reader wins and `inputs` stays unread; with only `inputs`, the scoped read fills it.
2. **Langflow** (scope `langflow`, exact):
   - **Kind**, in `scope_gated`: `trace_type` first: `agent` → `agent`, `tool` → `tool`, `retriever` → `retriever`, `embedding` → `embedding`, any other value (including `llm`, `prompt`, `chain`) → `chain`. Without `trace_type`, a present `flow_id` (the root) → `chain`.
   - **IO**, in `scoped.rs` (add the scope to the table): input from `inputs`, output from `outputs`, both the plain value (`non_message_payload`).
3. **n8n** (scope `n8n-workflow`, exact), in `scope_gated`:
   - `workflow.execute` → `chain`;
   - `node.execute` without `gen_ai.operation.name` → `span`;
   - with an operation, the existing GenAI rules decide.
   - The stored span name is not changed (D-IN-26 item 8).
   - n8n spans carry no content; nothing is read.

**Tests:**
- Prompt Flow: each `span_type` value; models from `llm.response.model` and `inputs.model`; a `Function` span whose `inputs` has a `model` field gets no model; an `inputs` without `model` leaves the request model empty; tokens (`llm.usage.prompt_tokens`, `completion_tokens`, `total_tokens`) fill an `llm` span and are not read on another scope or on a non-`llm` kind; VoltAgent's `llm.usage.*` still works; IO with and without `messages` and `choices`.
- Langflow: root with `flow_id`; each `trace_type`; a component span with `flow_id` and `trace_type = llm` is `chain`; IO as the plain value.
- n8n: `workflow.execute`, `node.execute` without and with `invoke_agent` and `chat`.
- Every rule: the same keys on another scope are unchanged.

**Done when:** as IN6.2.

**Commit:** `Read Prompt Flow, Langflow and n8n spans`

## IN6.4a — OpenLIT, Browser Use and Promptfoo rules

**Goal:** R-OL-1, R-OL-3 and R4 (Promptfoo). None needs an answer. Spec 02 changes in the same commit.

**Steps.**
1. **R-OL-1** (scope starts with `openlit.instrumentation.`), in `scope_gated`: when `gen_ai.operation.name` did not decide (its value is not in `genai_operation`), `decide(bag, "gen_ai.operation.type", …)`: `invoke_agent` → `agent`, `execute_task` → `chain`. Unit-tested only (D-IN-26 item 11).
2. **R-OL-3:** `browser_use` joins `FRAMEWORK_NAMES` in `context.rs` (sorted: it goes between `autogen` and `gcp.vertex.agent`; the test `framework_names_are_sorted` checks). Spec 02 has one list, the "Framework names, never providers" bullet in `## Normalization rules (clarifications)` (line ~277); it gains `browser_use`. Check that the `provider` row of `### Model, provider, usage` (line ~141) only points at that list. Unit-tested only.
3. **R4, Promptfoo** (scope `promptfoo.providers`, exact), in `scope_gated`: a span with `gen_ai.evaluation.name` is `evaluator`. The key is peeked, not consumed. Read `spikes/m6-tools-eval.md` § Evaluation tools (the Promptfoo table) for the keys. The request and response bodies are not read (D-IN-26 item 11).

**Tests:** each rule; the same keys on another scope are unchanged; `browser_use` is skipped as a provider and the next source decides.

**Done when:** as IN6.2.

**Commit:** `Read OpenLIT agent, Browser Use and Promptfoo spans`

## IN6.4b — Vector and zero-cost rules

**Blocked until Q1 and Q2 are answered.**

**Steps.**
1. **Vector spans** (Q1):
   - **OpenLIT** (scope starts with `openlit.instrumentation.`, `gen_ai.operation.name = vectordb`), in `scope_gated`:
     - (Q1 a) `retriever` when `db.operation.name`, lowercased, is `get`, `query` or `search`; `span` otherwise;
     - (Q1 b) `retriever` always.
   - **OpenLLMetry** (scopes `opentelemetry.instrumentation.chromadb`, `.qdrant`, `.milvus`, `.pinecone` and `.weaviate`), in `scope_gated`, by span name:
     - (Q1 a) these names give `retriever`, a fixed table in `kind.rs`:
       - Chroma: `chroma.query`, `chroma.get`;
       - Qdrant: `qdrant.query_points`, `qdrant.search`, `qdrant.scroll`;
       - Milvus: `milvus.search`, `milvus.query`, `milvus.get`;
       - Pinecone: `pinecone.query`, `pinecone.fetch`;
       - Weaviate: names starting with `db.weaviate.collections.query.` or `db.weaviate.gql.query.`;
     - (Q1 b) every span on those scopes gives `retriever`.
   - The Pinecone and Weaviate rows are unit-tested only (D-IN-26 item 11).
2. **Zero reported cost** (Q2), in `cost.rs` `apply`:
   - (Q2 a) after `reported_cost` returns, a reported total of exactly `0.0` (`Reported.total`, which is already the sum of the input and output parts when no total key is present) on a span with `usage.input_tokens` or `usage.output_tokens` above 0 is treated as no report. The keys are consumed as today and their values ignored (so a zero input or output part cost follows the same rule), and step 2 prices the span;
   - (Q2 b) the same only when the span's scope starts with `openlit`. `Charge` gains `scope: &str`; `normalize.rs` fills it;
   - (Q2 c) no code; the page documents it.

   Spec 02 § Cost computation step 1 and `docs/mvp-decisions.md` D109's text change for (a) and (b) (D109 gets "Amended by D-IN-26 item 14").

**Tests:** each rule; another scope is unchanged; the Q-dependent cases as answered. For Q2: a zero cost with tokens is priced and `cost_source` is `computed`; a zero cost without tokens stays `reported`; a non-zero reported cost stays; a zero cost with an unpriced model is `none`; zero part costs with tokens follow the rule.

**Done when:** as IN6.2.

**Commit:** `Read vector database spans and ignore a zero reported cost`

## IN6.5 — Captures A: Prompt Flow, LiveKit, Pipecat, Browser Use, MCP

**Goal:** five captured cases, each written from the spike's sketch:
- `spikes/m6-apps.md`: § 3 (Prompt Flow), § 4 (LiveKit), § 5 (Pipecat), § 6 (Browser Use 1.0);
- `spikes/m6-tools-eval.md`: § MCP, native case.

The spike scripts live in the scratchpad and may be gone. The dumps in the spike files are the reference.

**Steps.**
1. **Environments:**
   - Add Prompt Flow, LiveKit and Pipecat to `tools/capture/py` and run `uv lock`. Report every moved pin.
   - If the lock refuses one, give that case its own environment `tools/capture/envs/<promptflow|livekit|pipecat>/` (as IN6.1 step 7: project and lock files), put its scenario under `envs/<name>/scenarios/<group>/<case>/`, and report it.
   - Browser Use uses `envs/browser-use`; MCP uses `envs/mcp`. Their scenarios are under those environments.
   - A scenario module is named `<group>_<case>.py` with underscores (`livekit_text_session.py`); `main.py` imports it, as the existing scenarios do.
2. **Harness (LiveKit):** the plugin must see the host `api.openai.com`, so the cost is priced. In `tools/capture/harness/src/capture_py/harness.py`, add a sibling of `RealHostTransport` and `real_host_client` for async: `RealHostAsyncTransport(httpx2.AsyncBaseTransport)` and `real_host_async_client() -> httpx2.AsyncClient`, with the same rewrite of scheme, host and port to `mock_url()`. (`openai` 3.45.0, the capture project's pin, is built on `httpx2`; `httpx2.AsyncBaseTransport` and `AsyncHTTPTransport` exist in 2.16.0.) Run the harness trio.
3. **Scenarios.** Each uses the mock (`OPENAI_BASE_URL` or the library's `base_url`) and the runner's OTLP environment, and does what the spike sketch says:

   | Case | Program |
   |---|---|
   | `promptflow/openai-trace` | Two `@trace` functions around two chat calls (a tool call, then text); `setup_exporter_from_environ()` with `OTEL_EXPORTER_OTLP_ENDPOINT` set to the full traces URL (Prompt Flow posts to it as given) |
   | `livekit/text-session` | `AgentSession` text mode with one `get_weather` function tool and `set_tracer_provider(provider, allow_pii=True)`. The model is `openai.LLM(client=openai.AsyncClient(base_url="https://api.openai.com/v1", api_key=…, http_client=real_host_async_client()))`, or the plugin's equivalent: read `livekit-plugins-openai`'s source for how `LLM` takes a client and where `provider` comes from (the spike says `self._client._base_url.netloc`). **Stop** if the plugin cannot be given a client. If the captured `llm_request` span has no provider or host key at all, stop and report. The turns carry `"host": "api.openai.com"` |
   | `pipecat/openai-pipeline` | The aggregator pipeline with `OpenAILLMService`, a registered `get_weather`, `enable_tracing=True` and `conversation_id` |
   | `genai/browser-use-agent` | `browser-use` 1.0 with `pydantic_ai.Agent.instrument_all(True)` and plain OTel; headless Chromium from `PLAYWRIGHT_BROWSERS_PATH=<CETRA_CAPTURE_TOOLS>/playwright`; the mock returns the `done` JSON; `ANONYMIZED_TELEMETRY=false`, `BROWSER_USE_CLOUD_SYNC=false` |
   | `genai/mcp-tool` | **No chat, no mock turn (`"turns": []` in `scenario.json`).** The scenario runs MCP over stdio with the client and the server in separate processes (client in `main`, server a script of the scenario directory started with `sys.executable`), `mcp==2.3.0`, `instrumentation: "mcp"`. It calls `get_weather` once, then `tools/list`. Both processes export through `capture_py.harness.setup()`; the server flushes its provider before it exits, and the client waits for the server process to end before `finish`. The stdio transport hands the server only a short default environment, so the client passes the server the `OTEL_*`, `CETRA_CAPTURE_*` and `PATH` variables explicitly (`StdioServerParameters(env=…)`) |

   - Variables beyond the instrumentation switches in `scenario.json`'s `env` (whose keys must start with `OTEL_`, `OPENINFERENCE_`, `TRACELOOP_`, `LANGFUSE_`, `LMNR_`, `ANTHROPIC_` or `ADK_`) are set by the scenario code: for a subprocess in its `env`, for an in-process library with `os.environ[…] = …` before the import. `program_env` passes only a fixed list.
   - The mock turns follow existing tool-call scenarios, such as `openinference/openai-agents-tool`. Volatile and timing attributes are found by two runs, as in M5.
4. **Capture, bless, check.**
   - Commands: `just capture …`, `just fixtures-bless`, `just fixtures-fmt`, `just fixtures-check`.
   - Expected kinds come from the spike, adjusted by IN6.2 and IN6.3:
     - Prompt Flow: `span`, `llm` ×2, `span`;
     - LiveKit: `chain` (session), `span` (`start_agent_activity`), `agent`, `chain` (`llm_node`), `llm`, `span` (`llm_request_run`), `tool`, with exactly two `llm`; the `llm` is priced and its provider is `openai`; its `ttft_ns` is set;
     - Pipecat: `chain` ×2 (`conversation`, `turn`, both with the session id), `llm` ×2; `input` on both `llm`; `output` on the second `llm` only (the tool-call turn has none);
     - Browser Use: `agent`, `llm`, `tool`;
     - MCP: client `MCP send tools/call …` → `span`, server `tools/call get_weather` → `tool`, every other `mcp.*` method span → `span`; no `llm`.
   - Every `llm` with tokens is priced (`golden.rs`), and there is no `cetra.normalize_error`.
   - Stop and report any deviation.

**Done when:**
- `cargo nextest run -p cetra-otel --test golden --test fixtures`;
- `cargo run -q -p cetra-capture -- run <the five cases> --check`;
- the lint trio in the harness, in `tools/capture/py` and in each environment used.

**Commit:** `Capture Prompt Flow, LiveKit, Pipecat, Browser Use and MCP`

## IN6.6 — Captures B: OpenLIT and the vector databases

**Blocked until IN6.4b is committed.**

**Steps.** Write the scenarios from `spikes/m6-tools-eval.md` § OpenLIT, § Vector databases and § Capture plan. Modules are named `genai_openlit_chat.py`, `genai_openlit_vector.py` and `openllmetry_vector_databases.py`.

| Case | Environment | Program |
|---|---|---|
| `genai/openlit-chat` | `envs/openlit` | `openlit.init(otlp_endpoint=…, disable_metrics=True, capture_message_content=True, pricing_json=<scenario dir>/pricing.json, disabled_instrumentors=["httpx","requests","urllib3","aiohttp"])`; one chat with a tool call, the final turn, and one embedding call |
| `genai/openlit-vector` | `envs/openlit` | add, query, then delete on Chroma (in-memory), Qdrant (`:memory:`) and Milvus Lite (a temp file) |
| `openllmetry/vector-databases` | `envs/vector` | the same three operations on each, through OpenLLMetry's instrumentors |

- **OpenLIT pricing offline.** `pricing.json` in the scenario directory holds an empty table, so OpenLIT fetches nothing and no row matches: that is the Q2 case (`gen_ai.usage.cost = 0`). Check in `openlit`'s source that `{}` is an accepted `pricing_json`; if not, **stop**. The cost expectation follows Q2: (a) and (b) `cost_source = computed` with the table's price; (c) `reported` with `0`.
- `openlit` 1.45.0 needs `openai<3`, so the OpenAI client is built on `httpx`: the scenario points `base_url` at `mock_url("/v1")`; it does not need a real-host client.
- Add `openlit.agent.version_hash` to `volatile_attributes` (spike finding 9).
- Expected kinds:
  - `openlit-chat`: `llm` ×2, `embedding`, cost as answered by Q2;
  - vector cases: per Q1. OpenLIT's Milvus `DELETE` span is an error span although the delete worked (spike), and Qdrant and Milvus collection names are `unknown`: the fixtures keep both.

**Done when:** as IN6.5, for these three cases.

**Commit:** `Capture OpenLIT and vector databases`

## IN6.7 — Captures C: n8n, Langflow, Promptfoo

**Steps.** Write the scenarios from `spikes/m6-apps.md` §§ 1–2 and `spikes/m6-tools-eval.md` § Promptfoo. Each is a Python scenario in `tools/capture/py` that launches a tool from `target/capture-tools` with `harness.tool_binary`, as M5's gateways do. Use `free_port`, `wait_http(proc=…)` and `stop_gateway`. Modules are named `n8n_agent_webhook.py`, `langflow_basic_prompting.py` and `genai_promptfoo_eval.py`.

- **`scenario.json` of a tool scenario:** `instrumentation` is the tool's name (`n8n`, `langflow`, `promptfoo`); `libraries` is that name plus the Python libraries the scenario imports and records; the scenario ends with `finish_tool(None, {tool: version, **importlib_versions([…])})`, whose keys must cover every entry of `libraries`.
- **Export settings, all three tools.** Each scenario passes `OTEL_EXPORTER_OTLP_COMPRESSION=none` to the tool (or confirms from the first run that its bodies are identity: the receiver refuses gzip). Each waits a fixed `EXPORT_DELAY` after its last request, then calls `stop_gateway`. The runner's `PROGRAM_TIMEOUT` stays 180 s; if a case exceeds it, stop and report.
- **Environment of the tool process.** `scenario.json`'s `env` only takes the prefixes of IN6.5 step 3, and `program_env` passes a fixed list. Every variable below is set by the scenario code in the tool subprocess's `env` (the runner clears the parent's, so it also passes `PATH` and `HOME`, as `genai_litellm_proxy_chat.py` does).

1. **`n8n/agent-webhook`:**
   - Workflow: Webhook, then AI Agent, then OpenAI Chat Model, with the `openAiApi` credential's `url` on the mock.
   - Setup: `n8n import:workflow` and `import:credentials` from files in the scenario directory, then `update:workflow --active=true`, then `n8n start`. The binary is `tool_binary("n8n", <version>, "node_modules", ".bin", "n8n")`.
   - Environment: `N8N_USER_FOLDER` (a temp dir), `N8N_DIAGNOSTICS_ENABLED=false`, `N8N_VERSION_NOTIFICATIONS_ENABLED=false`, `N8N_TEMPLATES_ENABLED=false`, `N8N_AI_ENABLED=false`, the `N8N_OTEL_*` of the spike (`N8N_OTEL_ENABLED=true`, `N8N_OTEL_EXPORTER_OTLP_ENDPOINT` as the receiver origin, `N8N_OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`), `N8N_OTEL_TRACES_PRODUCTION_ONLY=false`, and `N8N_PORT` from `free_port()`.
   - Request: one `POST /webhook/<path>`.
   - Wait with a fixed `EXPORT_DELAY` (as `genai_litellm_proxy_chat.py` does), then `stop_gateway`.
   - Expected: `chain`, `span`, `agent`, `llm`. Every `node.execute` keeps its name (D-IN-26 item 8). There is no content.
2. **`langflow/basic-prompting`:**
   - `scenario.json` sets `"mock_host": "127.0.0.2"` (IN6.1): Langflow rewrites `127.0.0.1` when it thinks it runs in a container, so the mock binds `127.0.0.2`. The scenario sets `OPENAI_BASE_URL` from `mock_url("/v1")` and `LANGFLOW_PROVIDER_CREDENTIAL_ALLOWED_HOSTS` to `http://127.0.0.2` in the Langflow process's environment, plus the spike's `TRACELOOP_*`, `LANGFLOW_NATIVE_TRACING=false` and `DO_NOT_TRACK` variables.
   - The API sequence (sign-in or auto-login, API key, the flow built from the Basic Prompting starter with the model set, `POST /api/v1/run/<id>`) is derived by the implementer from the installed `langflow` 1.13.x source and docs, and written into the scenario's `notes`. Any step that needs a decision: **stop and report**.
   - Expected:
     - trace A: `chain` root and components;
     - trace B: one `llm`, a separate trace, as the page says.
3. **`genai/promptfoo-eval`:**
   - `promptfoo eval` with one prompt, one `contains` assertion and one `llm-rubric` assertion, against the mock's OpenAI routes.
   - Its OTLP export goes to the receiver: `PROMPTFOO_OTEL_ENABLED=true`, `PROMPTFOO_OTEL_ENDPOINT=<receiver origin>/v1/traces`.
   - Switches that turn its network use off (spike): `PROMPTFOO_DISABLE_TELEMETRY=1`, `PROMPTFOO_DISABLE_UPDATE=1`, `PROMPTFOO_DISABLE_SHARING=1`. Verify each spelling in promptfoo's docs or source; if it still makes a network call, **stop**.
   - A config directory override (the documented environment variable, `PROMPTFOO_CONFIG_DIR`: verify the spelling) pointing at a temp dir, and `--max-concurrency 1` so span order is stable.
   - The `llm-rubric` grader is a second model call: the mock turn for it returns the grader's JSON, `{"pass": true, "score": 1, "reason": "…"}`. Verify the exact JSON promptfoo's grader expects in its source before writing the turn.
   - Expected: `llm` ×2, `evaluator` ×2, `span`.

**Done when:** as IN6.5, for these three cases.

**Commit:** `Capture n8n, Langflow and Promptfoo`

## IN6.8 — Pages

**Goal:** the pages listed in § What it is. The captured pages follow the M5 captured-page shape (`litellm-proxy.mdx`).

**Edit:**
- `website/src/content/docs/docs/integrations/<slug>.mdx` (new), one per row below;
- `website/src/content/docs/docs/reference/attributes.md`;
- `website/src/content/docs/docs/concepts/conventions.mdx`;
- `website/src/content/docs/docs/tracing/cost-and-tokens.mdx` (Q2 (a) or (b) only);
- `skills/cetra/references/*.md` that the regeneration rewrites.

**Pages** (slug is the file name without `.mdx`; the title is the front-matter `title`):

| Slug | Title | Tier |
|---|---|---|
| `n8n` | n8n | captured |
| `langflow` | Langflow | captured |
| `prompt-flow` | Prompt Flow | captured |
| `livekit-agents` | LiveKit Agents | captured |
| `pipecat` | Pipecat | captured |
| `browser-use` | Browser Use | captured |
| `mcp` | MCP | captured |
| `openlit` | OpenLIT | captured |
| `vector-databases` | Vector databases | captured (Chroma, Qdrant, Milvus); Pinecone and Weaviate sections setup only |
| `promptfoo` | Promptfoo | captured |
| `stagehand` | Stagehand | setup only |
| `lobechat` | LobeChat | setup only |
| `gradio` | Gradio | setup only, points at the client library's instrumentor |
| `ragas` | Ragas | setup only |
| `uqlm` | UQLM | setup only |
| `azure-ai-evaluation` | Azure AI Evaluation | setup only |
| `cleanlab` | Cleanlab | setup only |
| `harbor` | Harbor and Terminal-Bench | setup only |

Captured pages have:
- a `<Backing>` box naming the fixtures;
- What you get, from `expected.json`;
- setup as built;
- Limits, from the spikes' findings:
  - n8n has no content, and every node span is named `node.execute`: the node name is in `n8n.node.name`;
  - Langflow's model call is a separate trace;
  - Prompt Flow needs the full `/v1/traces` URL;
  - LiveKit's voice spans are not captured, and `allow_pii` must stay at its default;
  - Pipecat has no tool span and no output on a tool-call turn, and the model calls carry no session;
  - Browser Use: 1.0 is Pydantic AI's spans; the 0.x line goes through OpenLIT;
  - OpenLIT: pin `>=1.37`, disable the HTTP instrumentors, and its `pricing_json`;
  - MCP: `mcp>=2` native, no content; the `mcp` registry row's role (IN6.S);
  - the vector databases: which operations are retrievers (Q1), OpenLIT's Milvus `DELETE` error span and its `unknown` collection names;
  - Promptfoo: its own switches.

Setup-only pages follow M5's setup-only pages. Every `python`, `ts` or `js` fence in them is preceded by `{/* untested: <reason> */}` (as `ag2.mdx` and M5's pages do); `check-samples.mjs` fails otherwise.

**Steps.**
1. **Setup-only pages:**
   - Stagehand: the v4 `telemetry` option. Its spans are OTLP/JSON, and no LLM span was seen.
   - LobeChat: unverified, `ENABLE_TELEMETRY=1`.
   - Gradio: points at OpenInference, OpenLLMetry or OpenLIT for the client library.
   - Ragas, UQLM, Azure AI Evaluation (Q4 a): trace the model calls with the provider's integration, then send each result with `cetra.score`. The sample code follows the existing scores docs; check it against `sdks/python/src/cetra`'s exports.
   - Cleanlab: the same; it needs a cloud key.
   - Harbor and Terminal-Bench: `harbor trace export --format otel --encoding pb --output traces/`, then POST each file as `application/x-protobuf` with the bearer header. Not run.
2. **Captured pages:** as above, after IN6.5 to IN6.7. The OpenLIT page and `tracing/cost-and-tokens.mdx` follow Q2.
3. **Docs edits:**
   - `reference/attributes.md`: the new keys read (`span_type`, `trace_type`, `lk.pii.*`, `llm.usage.*` and `llm.response.model` on scope `promptflow`, `gen_ai.operation.type`, `gen_ai.evaluation.name` on `promptfoo.providers`, `conversation.id` on `pipecat.turn`, and the rest);
   - `concepts/conventions.mdx`: one sentence per scope-gated family, as M5 did for Bifrost;
   - `tracing/cost-and-tokens.mdx`: for Q2 (a) or (b), the "Reported" bullet says a reported cost of exactly 0 on a span with tokens is priced from the table;
   - then `just docs-gen` (or `node scripts/skill-references.mjs`, as M5).

**Done when:** as IN5.4.

**Commits:**
- `Add setup-only app and tool pages`, which may land early;
- `Add app and tool pages`.

## IN6.9 — Index, sidebar, records

**Goal:** as IN5.5.

**Edit:** as IN5.5: `website/src/content/docs/docs/integrations/index.mdx`, `website/astro.config.mjs`, `website/src/marketing/data/integrations.test.mjs`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md`, `skills/cetra/references/integrations.md` (generated).

**Steps.**
1. **`index.mdx`:**
   - Add an **Apps and tools** section after Frameworks: rows, tiers, and cards for tested rows.
   - Add the matching sidebar group in `astro.config.mjs`.
   - Update `integrations.test.mjs`'s expected groups.
   - Read `src/pages/integrations.astro` to confirm the marketing page needs no other change.
2. **Design and `mvp-decisions.md`:**
   - D-IN-26, with Q1–Q4's answers (items 13 to 15 are Q1–Q3's);
   - amend the "New fixture group names" bullet (`00-design.md` line 84): `promptflow` and `langflow` are added, and `check-backing.mjs` now accepts `[a-z0-9_]+`;
   - amend D-IN-6 (design line 306, `mvp-decisions.md` line 1023): plain `output`/`inputs` are also read on scope `promptflow`, and `input`/`output` on scopes starting `pipecat`, by `conventions/scoped.rs` without a convention (D-IN-26 item 9);
   - amend D-IN-7's `mcp (M6)` cell: the row is added by M6;
   - the § M6 rows as built;
   - § Not supported gains the list in § What it is;
   - the appendix rows for M6 resolved.
3. **CHANGELOG:**
   - `Added`: the pages, the captured integrations, `instrument=["mcp"]` and the `cetra-sdk[mcp]` extra, and the capture harness package with per-environment scenario roots;
   - `Changed`: the scope-gated rules, `tts`/`stt` as spans, LiveKit's provider host (`api.openai.com` gives `openai`), and Q2's zero-cost rule if (a) or (b).
4. **Roadmap:** the M6 plan link, status "M1–M6 implemented", decisions `D-IN-1`–`D-IN-26`.
5. **Regenerate:** `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` (`just docs-gen`): the index changes `skills/cetra/references/integrations.md`. Commit what it rewrites.

**Done when:** as IN5.5.

**Commit:** `List the app and tool pages and record D-IN-26`

## After the last phase

`just check`, a Sonnet code review, a GLM agent for the fixes, then a push to PR #242.

## Amendment A — captures run offline; the receiver answers HEAD

IN6.7 found three problems:
- **Promptfoo:** with `PROMPTFOO_DISABLE_TELEMETRY=1` it still POSTs a `feature_used: telemetry disabled` event to `r.promptfoo.app`.
- **Langflow:** 1.13.2 always fetches `https://pypi.org/pypi/langflow/json` at start-up. No switch turns it off.
- **n8n:** 2.48.4 probes `HEAD <endpoint>/v1/traces` before exporting.

The decisions below are recorded in D-IN-26, item 11.

1. **Every capture runs in a private network namespace with only loopback,** on Linux. The recipes `just capture`, `just capture-all` and `just check-capture` run `cetra-capture` under `unshare --user --map-root-user --net`. The runner, mock, receiver and program then share one namespace, and nothing reaches the internet.
   - Loopback must be up inside the namespace. If `unshare` does not bring it up, the recipe runs `ip link set lo up` first. Verify which is needed, and name it.
   - Set `UV_OFFLINE=1`, so `uv run --frozen` never tries the index.
   - Tool installs (`just capture-tools`) stay outside the namespace.
   - A host without unprivileged user namespaces fails with a clear message. Captures are not in CI.
   - `AGENTS.md`'s capture lines say so.
   - Check that every existing case still passes `run --all --check` inside the namespace: `real_host_*` clients route to the mock and need no network. Any case that fails is a stop-and-report.
2. **The receiver answers `HEAD` on `/v1/traces`, `/v1/metrics` and `/v1/logs`** with 200 and an empty body. It records no error. Test: `a_head_probe_is_answered`.
3. **Langflow:** inside the namespace, its PyPI check fails with a connection error, which `build_version_notice` suppresses. Verify that, then run the case. Set `LANGFLOW_MODELS_DEV_REFRESH=false`.
4. **Promptfoo:** recapture inside the namespace. The case has three `span` spans (the test case and two provider wrappers), not one.

**IN6.7's Edit list gains:**
- `justfile` (the capture recipes);
- `tools/capture/src/receiver.rs`;
- `AGENTS.md`;
- this plan file.

5. **Span order is not a difference in `--check`.** Langflow runs two components at once, so the spans of one trace can end in either order. `json_differences` (`tools/capture/src/run.rs`) sorts each scope's `spans` by name, then by the span's canonical JSON, on both sides, after the time and timing values are masked. No fixture is rewritten. Test: `check_ignores_span_order` (reordered spans compare equal; a changed span is still reported).

6. **`unordered_spans` (scenario.json, default false).** For a program that ends its spans in any order and whose span ids change between runs (Langflow: two components run at once, and each component's span id moves with it). `json_differences` masks `spanId` and `parentSpanId` on both sides, then sorts the spans (point 5). Check-only: the fixture keeps its ids. Set in `langflow/basic-prompting`. Tests: `unordered_spans_mask_span_ids_in_the_check`, and the default still reports an id change. Documented in `docs/plans/b2-capture/README.md`.

7. **Captures run with a throwaway `/root`.** Under `--map-root-user`, aigw's `envoy-gateway certgen` writes to the passwd home of uid 0 (`/root/.config`), and it ignores HOME and XDG_CONFIG_HOME. Without the fix it writes into the real home (`/home/sebastien/.config/envoy-gateway`, which earlier aigw runs created), or fails. The namespace wrappers in `just capture`, `capture-all` and `check-capture` now add `--mount` and run `mount -t tmpfs tmpfs /root` after `ip link set lo up`. Verified: the mount works, and certgen then writes inside the tmpfs. aigw's version lookup, resolved. The gateway's envoy-gateway infra runner calls func-e's `api.Run` with `EnvoyVersion` set from the default image (`envoyproxy/envoy:distroless-v1.39.1`, so 1.39.1) and no `EnvoyVersionsURL`, so the URL can't be overridden through aigw (func-e v1.6.0 `api/run.go`; aigw's binary uses no `api.EnvoyVersionsURL`). func-e does not fetch the versions list when the binary is installed: `internal/envoy/install.go` `InstallIfNeeded` stats `EnvoyVersionsDir()/<version>/bin/envoy` and skips the lookup when it exists. The fix is the layout. When ConfigHome, DataHome, StateHome and RuntimeDir are one directory, func-e is in legacy mode (`internal/run/run.go` `initOpts` and `internal/runtime/opts.go` `InitializeGlobalOpts`; `internal/globals/paths.go` `EnvoyVersionsDir`): binaries live in `$DataHome/versions`, not `envoy-versions`. The scenario placed the binary in `envoy-versions`, which the lookup never checked. It now places it in `versions/1.39.1/bin/envoy` (`ENVOY_VERSION` names the version). Checked: aigw listens with no archive request, and the case passes `--check` in the namespace. No override and no committed versions list is needed.

## Amendment B — review fixes (after IN6.8's first half)

A Sonnet review of M6 so far found the items below. One agent fixes them all in one commit, `Fix the M6 review's findings`.

**Edit:**
- `crates/otel/src/{context,usage}.rs`
- `docs/spec/02-ingestion.md`
- `tools/capture/src/run.rs`
- `tools/capture/fetch-tools.sh`
- `justfile`
- `AGENTS.md`, if the capture lines change
- `website/src/content/docs/docs/integrations/{mcp,prompt-flow}.mdx`
- `sdks/python/pyproject.toml`, a comment only
- this plan file

1. **`livekit_provider`.** The value is a host only when one of these holds:
   - it contains `:`;
   - it parses as an IP address;
   - it equals `api.openai.com`;
   - `match_endpoint` maps it.

   A matched host gives its provider and consumes the key. An unmatched IP or host:port gives no provider and consumes the key, as today. Any other value, such as `azure.ai.openai` or `aws.bedrock`, is not a host: the key is left alone, and the normal provider path reads it.

   Tests: `azure.ai.openai` stays the provider; `gcp.vertex_ai` stays the provider; `127.0.0.1:1234` gives none; `api.openai.com` gives `openai`. Spec 02's LiveKit sentence changes to match.
2. **Span order.** `json_differences` sorts spans only when `scenario.unordered_spans` is true. `check_ignores_span_order` sets it.
3. **Span ids under `unordered_spans`.** After sorting by name, replace each `spanId` by a placeholder `span-<index>`. Replace each `parentSpanId` by its parent's placeholder, or `null` when there is no parent, through the same map. A re-parented span then still differs.

   Test: `unordered_spans_still_report_a_changed_parent`.
4. **Prompt Flow embeddings.** `scoped_usage` reads `llm.usage.*` on scope `promptflow` for `Llm` and `Embedding` spans. Add a test, and update spec 02's row.
5. **Chromium marker.** `fetch-tools.sh` treats Chromium as installed only when `chromium-*/INSTALLATION_COMPLETE` exists, and when a copy of the browser-use `uv.lock` beside it matches. On a mismatch it reinstalls.
6. **The namespace wrapper** becomes one private `justfile` recipe, used by `capture`, `capture-all` and `check-capture`. It must:
   - build `cetra-capture` first, outside the namespace (`cargo build -q -p cetra-capture`);
   - probe, in one check, that `unshare --user --map-root-user --net --mount` works, that `ip` exists and that a tmpfs can be mounted on `/root`. On failure, print the command's own stderr, then the "see AGENTS.md" line;
   - mount the tmpfs on `/root` only when `$HOME` is not `/root` and does not start with `/root/`, and say so in a comment.
7. **`mcp.mdx`:**
   - The OpenInference row does not create spans. It carries the trace context to the server, so spans that the server process makes (with its own instrumentation, or with spans the user writes) join the client's trace. On `mcp<2` the MCP SDK makes no spans itself.
   - Not backed by a fixture: say so.
   - Whether `openinference-instrumentation-mcp` 2.1.5 runs beside `mcp>=2`: the spike saw a harmless `ValueError` and no effect. Say that, and that it is not needed there.
8. **SDK pin comment.** Above `exclude-newer-package`, add: "Remove once openinference-instrumentation-mcp's floor is raised past a version older than 7 days."

**Done when:**
- `cargo nextest run -p cetra-otel -p cetra-capture`, with clippy and fmt;
- `just fixtures-check`, with no `expected.json` changed;
- `bash -n tools/capture/fetch-tools.sh`;
- one `just capture genai/mcp-tool` through the new wrapper;
- from `website/`: `check-terms` and `skill-references`.
