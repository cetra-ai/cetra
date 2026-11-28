# IN M9 — Long-tail frameworks

*Cold review (Sonnet) rounds 1 and 2 applied to IN9.1, IN9.4, IN9.5 and § Versions; IN9.2 is rewritten from spike F (`m9-a` § "Facts for the capture phases").*

This plan closes the integrations track's matrix with D-IN-31's dispositions: captured cases and pages for Vercel's eve, Oracle Agent Spec, Atomic Agents, TanStack AI and Flue; two normalizer rules (eve's Workflow SDK spans, Agent Spec's `"null"` tokens); setup-only pages for Cloudflare Agents, Graphite, NVIDIA NeMo Agent Toolkit and Pixeltable; a Claude Managed Agents section on the Anthropic page; a LangServe note on the LangChain page; and § Not supported rows. EverOS gets a captured case and page, and Cognee a setup-only page (IN9.6, from spike E).

**Prerequisites:** M8 done (commits up to 869f2d7). IN9.4's four setup-only pages and the LangServe note are already committed (62dadc9). D-IN-31 committed in `00-design.md`.

**Where things run:** builds and tests may run on the SSH host `ubuntu-dev` (aarch64; `ssh ubuntu-dev 'cd ~/cetra && …'` after `git push ubuntu-dev claude/in-integrations`). Captures run on the local machine (Linux x86_64; `tools/capture` and its pinned tools are x86_64 only).

**Read first:**
- `AGENTS.md`.
- `docs/plans/in-integrations/00-design.md`: D-IN-23 item 10 (fixture groups), D-IN-26 items 1, 2, 11, D-IN-28 item 8, **D-IN-31**.
- The spikes: `spikes/m9-a-agent-frameworks.md` (eve, AgentSpec, Atomic Agents, Flue, NeMo, Graphite, and the follow-ups), `spikes/m9-b-rag-data.md` (TanStack AI, Pixeltable, LangServe), `spikes/m9-c-platforms.md` (Claude Managed Agents, Cloudflare Agents).
- The spike programs, committed under `spikes/m9-files/` (fake keys `sk-fake-m9`; never copy them): `eve/` (the eve project's `agent/` folder, `package.json`, `eve_run.sh`), `agentspec/ags_run.py`, `atomic-agents/aa_run.py`, `tanstack/run-tanstack.mjs` and `package.json`, `flue/run-flue.mjs` and `package.json`.
- The code: `crates/otel/src/normalize.rs` (the `agents::discarded` call site), `crates/otel/src/conventions/openinference.rs` and `crates/otel/src/usage.rs` (`one_count`), `tools/capture/ts/{agents.ts,harness.ts}`, `tools/capture/ts/scenarios/genai/mastra-agent-tool/` (a TS GenAI scenario), `tools/capture/py/scenarios/` and `tools/capture/envs/` (Python scenarios and per-framework environments, D-IN-26 item 1).

## What it is, and what it is not

**When it is done:**
- Two normalizer rules (§ Rules), with unit tests and spec 02 text.
- Captured cases: `genai/eve-agent-tool`, `openinference/agentspec-agent`, `genai/atomic-agents-agent-tool`, `openinference/tanstack-ai-tool`, `genai/flue-agent-tool`.
- Pages: `eve`, `agent-spec`, `atomic-agents`, `tanstack-ai`, `flue` (captured); `cloudflare-agents`, `graphite`, `nemo-agent-toolkit`, `pixeltable` (setup only); a "Claude Managed Agents" section on `anthropic.mdx`; a LangServe note on the LangChain page.
- Index rows, sidebar entries, § Not supported (already in the design doc), D-IN-31 in `mvp-decisions.md`, CHANGELOG, roadmap; the track's roadmap row marked done.

**Not in this plan:** a Cognee capture (its run did not complete offline); pricing LiteLLM-style `openai/`-prefixed model names (a follow-up); TanStack's native `otelMiddleware` as a captured case (D-IN-31 item 6); folding eve's Workflow SDK spans into one span (rejected in D-IN-31).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-6 | Discarded spans are counted, never stored or rejected |
| D-IN-23 item 10 | Native GenAI frameworks in group `genai`, OpenInference ones in `openinference` |
| D-IN-26 items 1, 2, 11 | Per-framework Python environments when locks conflict; tool projects; captures offline in a loopback namespace |
| D-IN-28 item 8 | Pins win; later pins are the newest release ≥ 7 days old |
| D-IN-31 | This milestone's dispositions and rules |

The user's answers (2026-11-24): all spike recommendations; the eve and Agent Spec rules; TanStack through OpenInference's middleware; the Cognee and EverOS tokenizer downloads for spike E.

## Rules (D-IN-31 item 5)

| Rule | Where | Behaviour |
|---|---|---|
| Workflow SDK spans | `crates/otel/src/agents/mod.rs`, a new `pub(crate) fn scoped_discard(scope_name: &str, span_name: &str) -> bool` next to `discarded()` (it is a discard, like the coding agents'), called in `normalize.rs` right after the `agents::discarded` check (~line 217) and adding to `tally.discarded` the same way | `true` when the scope is exactly `workflow` and the name starts with `workflow.`, `step.` or `queue.` (Vercel's Workflow SDK, which eve runs on). Any other scope or name: `false`. |
| `"null"` tokens | `crates/otel/src/usage.rs` `usage()` (~line 319; signature today `(bag, kind, provider, scope_name, conventions, warnings)`): add a `span_name: &str` parameter after `kind`, pass `&span.name` at the one non-test call site (`normalize.rs` ~271), and update every `usage(` call in `usage.rs`'s tests; the rule runs before the `first_count` calls (~line 332) | Agent Spec's spans have no instrumentation scope ( — `spikes/m9-a-agent-frameworks.md` § "Facts for the capture phases (spike F)" item 1a), so the rule is gated on its model-call span name: when the span name is exactly `LlmGenerationSpan` and `kind` is `llm`, for each of `llm.token_count.prompt`, `llm.token_count.completion`, `llm.token_count.total`, `llm.token_count.prompt_details.cache_read`, `llm.token_count.prompt_details.cache_write`, `llm.token_count.completion_details.reasoning` whose value is exactly the string `"null"` (case-sensitive), `bag.take(key)`: consumed, no count, no `invalid_number`. Every other span name keeps today's behaviour (rule 20). |

## Versions

The implementer re-checks each publish time and reports the version used. These pins win (D-IN-28 item 8).

| Package | Version | Where |
|---|---|---|
| `eve` (npm) | 1.0.1 | `tools/capture/ts/package.json`, or a tool project if its dependencies conflict (IN9.0) |
| `@tanstack/ai` | 1.5.0 | `tools/capture/ts/package.json` |
| `@tanstack/ai-openai` | 1.1.1 | same |
| `@arizeai/openinference-tanstack-ai` | 0.3.5 | same |
| `@flue/runtime`, `@flue/opentelemetry` | 2.3.0 | same |
| `@earendil-works/pi-ai` (for Flue only) | 1.4.0 | a direct dependency of `tools/capture/ts` (the Flue program imports it; pnpm's strict layout needs it direct) plus the scoped override `"@flue/runtime>@earendil-works/pi-ai": "1.4.0"`; pi's capture keeps its own 1.5.x through `@earendil-works/pi-coding-agent` |
| `@ai-sdk/openai-compatible` 3.0.92, `@vercel/otel` 2.3.0, `zod` 4.6.7 (eve's agent files; `ai` 7.0.173 comes transitively) | as listed (`spikes/m9-a-agent-frameworks.md` § "Facts for the capture phases (spike F)" item 4d) | `tools/capture/ts/package.json` |
| `valibot` (Flue's tool schema, `spikes/m9-a-agent-frameworks.md` § "Facts for the capture phases (spike F)" item 3a) | the newest ≥ 7 days old, looked up with `npm view valibot time --json` and recorded | same |
| OpenTelemetry JS | the capture project's existing pins (`@opentelemetry/api` override 1.9.0, `sdk-trace-node`, `exporter-trace-otlp-proto`) | unchanged; report any peer warning the new packages print |
| `pyagentspec` | 26.3.1 | `tools/capture/py` or an `envs/agentspec` environment (IN9.0) |
| `openinference-instrumentation-agentspec` | 0.1.22 | same |
| `atomic-agents` | 2.16.0, without the `[phoenix]` extra (its spans keep every GenAI attribute without it; `spikes/m9-a-agent-frameworks.md` § "Facts for the capture phases (spike F)" item 2a) | `tools/capture/py` or an `envs/atomic-agents` environment (IN9.0) |
| `everos` | 1.7.0 with its `otel` extra (`everos[otel]`; without it the server exports nothing) | a uv tool project `tools/capture/everos/` in the shape of `tools/capture/litellm-proxy/` (it runs as a server) |
| tiktoken `o200k_base` (EverOS) | sha256 `446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d`, from `https://openaipublic.blob.core.windows.net/encodings/o200k_base.tiktoken` (user-approved, 2026-11-24) | fetched by `fetch-tools.sh` into `target/capture-tools/tiktoken/`, checked by sha256, cached under tiktoken's cache-file name `fb374d419588a4632f3f557e76b4b70aebbca790` |

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN9.0 Installs | `tools/capture/ts/{package.json,pnpm-lock.yaml,pnpm-workspace.yaml}`, `tools/capture/py/{pyproject.toml,uv.lock}` or new `tools/capture/envs/{agentspec,atomic-agents}/{pyproject.toml,uv.lock}` | — |
| IN9.1 Rules | `crates/otel/src/agents/mod.rs`, `crates/otel/src/normalize.rs`, `crates/otel/src/usage.rs`, `docs/spec/02-ingestion.md` | — |
| IN9.2a–e Captures (one per case) | new scenarios under `tools/capture/ts/scenarios/{genai,openinference}/` (eve, TanStack, Flue) and `tools/capture/py/scenarios/{openinference,genai}/` or `tools/capture/envs/*/scenarios/…` (Agent Spec, Atomic Agents), `.gitignore` (eve's `.eve/`), new fixtures | IN9.0, IN9.1 |
| IN9.3 Captured pages | new `website/src/content/docs/docs/integrations/{eve,agent-spec,atomic-agents,tanstack-ai,flue}.mdx` | IN9.2 |
| IN9.4 Claude Managed Agents section | `website/src/content/docs/docs/integrations/anthropic.mdx` | — |
| IN9.6 EverOS | `tools/capture/fetch-tools.sh`, `tools/capture/src/scenario.rs` (the `dims` bound), new `tools/capture/everos/{pyproject.toml,uv.lock}`, a new TS scenario `tools/capture/ts/scenarios/genai/everos-memory/`, new fixtures, new `website/src/content/docs/docs/integrations/everos.mdx`, new `website/src/content/docs/docs/integrations/cognee.mdx` (setup only) | IN9.1 |
| IN9.5 Index and records | `index.mdx`, `astro.config.mjs`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md`, `skills/cetra/references/*` (generated) | IN9.3, IN9.4 (and IN9.6 when written) | (and IN9.6)

**Parallel runs:** IN9.0, IN9.1 and IN9.4 can run at the same time (no shared files). IN9.2a–e after IN9.0 and IN9.1, one after another (one capture run at a time; each commits on its own). IN9.6 after IN9.1, not beside a capture phase.

---

## IN9.0 — Installs

**Goal:** the pinned packages are installed for the capture scenarios.

**Steps.**
1. TypeScript: add the § Versions npm packages (exact) to `tools/capture/ts/package.json`, with the scoped override for Flue in `pnpm-workspace.yaml` (`overrides`, as existing entries are written there). Run `pnpm install` in `tools/capture/ts` (not frozen, to update the lock), then `just capture-ts-sync`. If pnpm refuses a transitive version as younger than 7 days, or the override does not take (check `node_modules/.pnpm` for `@earendil-works+pi-ai@1.4.0` under `@flue/runtime` and pi's 1.5.x still used by `@earendil-works/pi-coding-agent`), stop and report. Install scripts: list every new package's install script in the report; allow (in `pnpm-workspace.yaml` `allowBuilds`, as exact `name@version` entries like the existing ones) only what an import needs, and say why.
   - If `eve`'s dependency tree conflicts with the capture project's (pnpm errors, or peer conflicts that break an existing scenario's `--check`), make it a pnpm tool project `tools/capture/eve/` in the shape of `tools/capture/openclaw/`, installed by `pnpm_tool eve 1.0.1 eve/package.json` in `fetch-tools.sh` (add that file to this phase's Edit list in your report).
2. Python: add `pyagentspec==26.3.1`, `openinference-instrumentation-agentspec==0.1.22` and `atomic-agents==2.16.0` to `tools/capture/py`'s dependencies and run `uv lock`. If the lock cannot resolve with the existing frameworks (`exclude-newer`, conflicting pins), create `tools/capture/envs/agentspec/` and/or `tools/capture/envs/atomic-agents/` in the shape of an existing `envs/*` project (D-IN-26 item 1) instead, and say which. Agent Spec's LangGraph adapter needs `langgraph` and `langchain-openai`: take them as `pyagentspec`'s own extra if it has one (read its metadata), else the versions the spike installed (`spikes/m9-a-agent-frameworks.md` § AgentSpec), and record them.
3. eve smoke test: in a temporary folder under `tools/capture/ts/scenarios/` (deleted afterwards), copy `spikes/m9-files/eve/agent/` and a `package.json` with `{"name": "capture-eve", "type": "module"}`, then run `node tools/capture/ts/node_modules/eve/bin/eve.js dev --no-ui --port <free port>` from it for 15 s and confirm it reaches "server listening" (module resolution from `tools/capture/ts/node_modules`). If it does not, use the tool-project fallback of step 1 and record the path to `eve.js`.
4. Run `just capture-tools`, `just capture-py-sync`, `just capture-ts-sync` twice each (second run up to date), and `just check-capture` once: every existing case still passes `--check`.

**Done when:** the installs succeed twice and `just check-capture` passes.

**Commit:** `Pin eve, Agent Spec, Atomic Agents, TanStack AI and Flue for captures`

## IN9.1 — Rules

**Goal:** the two rules of § Rules.

**Steps.**
1. `agents/mod.rs`: `scoped_discard` with constants for the scope and the three prefixes, and a doc comment naming eve, Vercel's Workflow SDK and D-IN-31 item 5.
2. `normalize.rs`: after the `agents::discarded` check, `if agents::scoped_discard(scope_name.as_deref().unwrap_or(""), &span.name) { tally.discarded += 1; return None; }`.
3. `usage.rs`: the `"null"` rule as § Rules says, with `AGENTSPEC_LLM_SPAN = "LlmGenerationSpan"` and the key list as constants, and a doc comment naming D-IN-31 item 5 and saying Agent Spec's spans have no scope.
4. Spec 02 (`docs/spec/02-ingestion.md`): the discard as a new sentence in § Coding agents "Other discards" (~line 329) saying it is not a coding agent but uses the same mechanism; the `"null"` rule as an amendment to rule 20 of § Normalization rules (~line 300), naming the span.

**Tests:**
- In `agents/mod.rs`'s tests, with `test_support`'s `span`, `request` and `run` (as the OpenClaw discard tests do): `workflow_sdk_spans_are_discarded` (scope `workflow`, names `workflow.start`, `step.execute`, `queue.publish`: all discarded, `discarded` is 3); `workflow_scope_other_names_are_kept` (scope `workflow`, `chat gpt-4o-mini`); `workflow_names_on_another_scope_are_kept` (scope `eve.agent`, `workflow.start`).
- In `usage.rs`'s tests, with its `int_attr`/`string_attr` helpers (as the existing `counted` tests): `agentspec_null_tokens_are_absent_without_warning` (span `LlmGenerationSpan`, empty scope, kind llm, all six keys `"null"`: no token columns, no `invalid_number`, keys consumed); `agentspec_numeric_tokens_are_read` (82 and 17 on that span are read); `null_tokens_on_another_span_still_warn` (span `generate`, same keys).

**Done when:** `cargo nextest run -p cetra-otel`, clippy (`-D warnings`) and `cargo fmt -p cetra-otel --check` pass (on ubuntu-dev or locally); no `expected.json` changes.

**Commit:** `Discard eve's Workflow SDK spans and read Agent Spec's null tokens as absent`

## IN9.2 — Captures (IN9.2a–e)

Five phases, one case each, run one after another; each ends with its own `--check`, bless and commit. Common to all five:

- **Where facts come from:** `spikes/m9-a-agent-frameworks.md` § "Facts for the capture phases (spike F)" (spike F) and the spike programs in `spikes/m9-files/`. If a run disagrees with spike F, stop and report.
- **Program shape:** TypeScript cases follow `tools/capture/ts/scenarios/genai/mastra-agent-tool/` (`main.ts` + `scenario.json`; `writeMeta()` from `harness.ts` after the provider is flushed). Python cases live in the environments IN9.0 created (`tools/capture/envs/agentspec/scenarios/…`, `tools/capture/envs/atomic-agents/scenarios/…`, 1faaa33) and follow an existing `envs/*/scenarios/` case: `main.py` as the entry plus a uniquely named module (e.g. `agentspec_agent.py`), with mypy overrides for untyped packages added to the project's `pyproject.toml` (`[[tool.mypy.overrides]]`, as existing entries do; that file joins the phase's edits). In an `envs/<name>` environment (IN9.0's choice), the same shape under `envs/<name>/scenarios/`.
- **Environment variables** a framework reads (`OPENAI_BASE_URL`, `ATOMIC_AGENTS_TRACE_CAPTURE_MESSAGES`, …) are set inside the program (`os.environ[...]` before the framework loads; `process.env[...]` in TypeScript), never in `scenario.json` `env` (its allowed prefixes, `scenario.rs` ~52–60, reject them). The mock's base URL is `CETRA_CAPTURE_MOCK_URL` + `/v1` (the harness's `mock_url("/v1")` in Python).
- **Turns** use the API spike F recorded for each case. A turn's `model` is the response model; spans name the request model `gpt-4o-mini`. Usage 82/17 for the first turn, 131/12 for the second.
- **`scenario.json`:** `description`; `spec_refs` from real headings (`"02 § Mapping tables / Input / output"`, plus `"02 § Normalization rules"` for Agent Spec and `"02 § Coding agents"` for eve's discard); `instrumentation` and `libraries` as each case says; `volatile_attributes` / `timing_attributes` / `volatile_name_prefixes` / `span_names` / `unordered_spans` / `tolerated_probes` only as `--check` shows they are needed, each with a reason in `notes`. Use `timing_attributes`, not `volatile_attributes`, for long values whose substrings appear inside message content (869f2d7).
- **Each phase:** capture, `just _capture-namespace <case> --check` twice, `just fixtures-bless`, `fixtures-fmt`, `fixtures-check`; stop if the result differs from the case's Expected in kinds, tokens or provider. **Done when:** the case passes `--check`; `cargo nextest run -p cetra-otel -p cetra-capture` passes; the TypeScript typecheck and Biome (TS cases) or the Python lint and type checks `check-capture` runs (Python cases) pass.

### IN9.2a — Agent Spec (`openinference/agentspec-agent`, Python)

- Program: from `spikes/m9-files/agentspec/ags_run.py` and spike F item 1: the docs' example agent with one `ServerTool` `read_file(path: str)` whose function reads the file and returns its text, executed through `AgentSpecLoader(tool_registry={"read_file": <fn>})`; LangGraph adapter; `OPENAI_BASE_URL` set to the mock (the adapter ignores `OpenAiConfig.url`); `AgentSpecInstrumentor().instrument(tracer_provider=…)`. `hello.txt` with `hello from cetra\n` in the working directory; the tool call's `path` is `hello.txt`.
- Turns (`openai.chat`, non-streaming): turn 1 a tool call `read_file` with `{"path": "hello.txt"}`, finish `tool_calls`; turn 2 text `hello.txt says: hello from cetra`, finish `stop`.
- `instrumentation` `openinference-instrumentation-agentspec`; `libraries` `["openinference-instrumentation-agentspec", "pyagentspec"]`.
- Expected: agent `AgentExecution[…]`, llm `LlmGenerationSpan` ×2 with input and output messages, **no token columns and no `invalid_number`** (IN9.1), cost source `none`, tool `ToolExecution[read_file]`. The scope is empty.
- Commit: `Capture Agent Spec`.

### IN9.2b — Atomic Agents (`genai/atomic-agents-agent-tool`, Python)

- Program: from `spikes/m9-files/atomic-agents/aa_run.py` and spike F item 2, without the `[phoenix]` extra: an app span `turn` around one `agent.run(...)`, then the app calls the tool explicitly (`ReadFileTool().run(ReadFileInput(path="hello.txt"))`; name the input class `ReadFileInput`, since Atomic Agents names the tool span after it). `ATOMIC_AGENTS_TRACE_CAPTURE_MESSAGES=true` set in the program.
- Turn (`openai.chat`, non-streaming): **one** turn, a forced tool call named after the agent's output schema class (`BasicChatOutputSchema` unless the program defines its own; spike F item 2b) with arguments `{"chat_message": "I will read hello.txt."}`, finish `tool_calls`. No second turn (the agent makes one model call).
- `instrumentation` `atomic-agents`; `libraries` `["atomic-agents", "instructor"]`.
- Expected: `turn` (span), agent `invoke_agent AtomicAgent` (its repeated tokens not counted), llm `chat gpt-4o-mini` 82/17 with cost `computed`, tool `execute_tool ReadFileInput` as a child of `turn` with arguments and result.
- Commit: `Capture Atomic Agents`.

### IN9.2c — TanStack AI (`openinference/tanstack-ai-tool`, TypeScript)

- Program: from `spikes/m9-files/tanstack/run-tanstack.mjs`, mode `oi` (`openInferenceMiddleware({ tracer })`), the `openaiText("gpt-4o-mini", { baseURL: <mock>/v1 })` adapter, tool `read_file` reading `hello.txt`.
- Turns (`openai.responses`, streamed as SSE by the mock, as the spike's adapter called `/v1/responses`): turn 1 the tool call, turn 2 the text. The program uses the harness's `setup()`/`finish()` for its tracer provider, not the spike's own SDK setup.
- `instrumentation` `@arizeai/openinference-tanstack-ai`; `libraries` `["@arizeai/openinference-tanstack-ai", "@tanstack/ai", "@tanstack/ai-openai"]`.
- Expected: agent `ai.chat`, llm `ai.llm 1` (82/17) and `ai.llm 2` (131/12) with cost `computed`, tool `ai.tool read_file`.
- Commit: `Capture TanStack AI`.

### IN9.2d — Flue (`genai/flue-agent-tool`, TypeScript)

- Program: from `spikes/m9-files/flue/run-flue.mjs` and spike F item 3: `import { createProvider } from '@earendil-works/pi-ai'` and `import { openAICompletionsApi } from '@earendil-works/pi-ai/api/openai-completions.lazy'`; `createProvider({ id: 'openai', … })` with base URL `<mock>/v1`, models with `provider: 'openai'`; `useModel('openai/gpt-4o-mini')`; tool `defineTool({ name: 'read_file', input: v.object({ path: v.string() }), run: async ({ data }) => ({ output: <file text> }) })` with `import * as v from 'valibot'`; `@flue/opentelemetry`'s instrumentation exporting to the receiver.
- Turns (`openai.chat`, streaming with usage): turn 1 the tool call `read_file`, turn 2 the text. Flue also offers its built-in `task` tool; if it makes a model call the two turns do not cover, stop and ask. Use the harness's `setup()`/`finish()` for the tracer provider instead of the spike's `SimpleSpanProcessor`. If Flue's `start()` binds a port and a span carries it, the leak check refuses the case: stop and ask.
- `instrumentation` `@flue/opentelemetry`; `libraries` `["@flue/opentelemetry", "@flue/runtime", "@earendil-works/pi-ai"]`.
- Expected: agent `invoke_agent …`, llm `chat gpt-4o-mini` ×2 with provider `openai`, tokens and cost `computed`, tool `execute_tool read_file`.
- Commit: `Capture Flue`.

### IN9.2e — eve (`genai/eve-agent-tool`, TypeScript)

- Layout: the eve project lives in the scenario folder: `agent/agent.ts`, `agent/instructions.md`, `agent/tools/read_file.ts`, `agent/instrumentation/cetra.ts` (from `spikes/m9-files/eve/agent/`), and a `package.json` `{"name": "capture-eve", "type": "module", "private": true, "dependencies": {"eve": "1.0.1"}}` (the name sets the agent and service names, spike F item 4c; eve finds its project root at the first `package.json` upward and requires it to declare `eve`; `type` keeps `main.ts` an ES module). IN9.0 put eve in the tool project `tools/capture/eve/` (1faaa33: eve needs AI SDK 7, the capture project has 6): its binary is `$CETRA_CAPTURE_TOOLS/eve/1.0.1/node_modules/eve/bin/eve.js`, and eve bundles `agent/*.ts` at startup, so `main.ts` creates a symlink `node_modules` in the scenario folder pointing at `$CETRA_CAPTURE_TOOLS/eve/1.0.1/node_modules` before the spawn (and removes it in `finally`); add `tools/capture/ts/scenarios/genai/eve-agent-tool/node_modules` to `.gitignore`.
  - `agent.ts`: `createOpenAICompatible({ name: "openai", baseURL: process.env.CETRA_CAPTURE_MOCK_URL + "/v1", apiKey: process.env.OPENAI_API_KEY })` (provider reads `openai.chat`, mapped to `openai`).
  - `instrumentation/cetra.ts`: the exporter URL is `process.env.CETRA_CAPTURE_OTLP_ENDPOINT` (the full traces URL), with no `Authorization` header (drop the spike's).
  - `read_file.ts`: reads the `path` argument relative to eve's working directory and returns the file's text.
  - `.gitignore`: `tools/capture/ts/scenarios/genai/eve-agent-tool/.eve/`; `tools/capture/ts/tsconfig.json`: exclude `scenarios/genai/eve-agent-tool/.eve/**` (and `agent/**` if its typecheck fails on eve's own types; say why); check `pnpm exec biome check tools/capture/ts` ignores `.eve/` (Biome follows `.gitignore` when configured to; if not, add the folder to Biome's ignore in `biome.json`, and say so). These three files join the phase's edits.
- `main.ts`: make a temporary directory `work` (holds `hello.txt` with `hello from cetra\n`; eve itself runs from the scenario folder, where its `package.json` is, so `read_file.ts` resolves `path` against `process.env.CAPTURE_EVE_WORK`, which `main.ts` sets to `work`); find a free port; spawn `node <path to eve.js> dev --no-ui --port <port>` with cwd the scenario folder, not detached, and an explicit `env` of only `PATH`, `HOME`, `TZ`, `LANG`, `CETRA_CAPTURE_MOCK_URL`, `CETRA_CAPTURE_OTLP_ENDPOINT`, `OPENAI_API_KEY`, `CAPTURE_EVE_WORK`, and `XDG_CONFIG_HOME` and `TMPDIR` set to subfolders of `work` (no inherited `OTEL_*`, so the service name stays `capture-eve`); poll `GET /` until HTTP 200 (at most 30 s); `POST /eve/v1/session` with the prompt; read `GET /eve/v1/session/<id>/stream` until the line with `"type":"turn.settled"` (at most 60 s); wait 7 s (eve's last OTLP batch arrives about 5 s after the last model call); send SIGINT, wait up to 4 s for exit, then SIGKILL; await the exit; remove `work`; `writeMeta()`. The whole sequence after the spawn sits in `try`/`finally`, and `finally` SIGKILLs the child if it is still running.
- First run: read the capture's leak report before anything else. The leak check scans every received span, including the 51 Workflow SDK spans that IN9.1 later discards. If any span carries a loopback address, a port or a local path, stop and ask (the fix belongs to the capture tool, outside this phase).
- Turns (`openai.chat`): turn 1 the tool call `read_file` `{"path": "hello.txt"}`, finish `tool_calls`; turn 2 the text, finish `stop`.
- `instrumentation` `eve`; `libraries` `["eve", "@vercel/otel"]`, read with `writeMeta` aliases to the tool project (`{ eve: "../../../../target/capture-tools/eve/1.0.1/node_modules/eve", "@vercel/otel": "../../../../target/capture-tools/eve/1.0.1/node_modules/@vercel/otel" }`, as the OpenCode v1 case does).
- Determinism: across spike F's three runs only ids and timestamps varied. Expect `unordered_spans` and id masks; add them as `--check` shows.
- Expected: agent `invoke_agent capture-eve` (its summed usage not counted), `agent.step` ×2 (span), llm `chat gpt-4o-mini` ×2 with provider `openai` and cost `computed`, tool `execute_tool read_file`; no Workflow SDK span kept (the case's `discarded` count is 51).
- Commit: `Capture eve`.

## IN9.3 — Captured pages

**Goal:** five pages in the shape of an existing captured framework page (`mastra.mdx` or `pydantic-ai.mdx`): `<Backing fixtures={[...]} />`, setup (from the scenario program, with the user's endpoint and `sk-cetra-…` key placeholders from `website/src/content/docs/docs/get-started/opentelemetry.mdx`), What you get (from `expected.json`), Limits.

| Page | Limits (at least) |
|---|---|
| `eve` | cetra drops the Workflow SDK's spans (`workflow.*`, `step.*`, `queue.*`); some of them are in traces of their own |
| `agent-spec` | the LangGraph adapter reports no token counts (no cost); it ignores `OpenAiConfig.url` (set `OPENAI_BASE_URL`); the agent span's parent is not exported, so the agent span has a missing parent |
| `atomic-agents` | the tool span is named after the tool's input schema class; with a provider instrumentor also on, set `ATOMIC_AGENTS_TRACE_LLM_SPANS=false` (each call twice otherwise); message capture is off by default |
| `tanstack-ai` | the page's route is OpenInference's middleware; TanStack's own `otelMiddleware` also exports (seen in a spike run, not captured), but its root span repeats the run's tokens and is read as a model call, so tokens count twice |
| `flue` | pin `@earendil-works/pi-ai` to 1.4.0 (Flue 2.3.0 fails to import with 1.5.x); content is on by default |

Fences in Python and TypeScript are samples under `check-samples.mjs`: mark each `{/* untested: … */}` unless a test runs it (read the script and existing pages).

**Done when:** `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs` pass; `check-integrations.mjs` fails only for pages not yet in the index.

**Commit:** `Add eve, Agent Spec, Atomic Agents, TanStack AI and Flue pages`

## IN9.4 — The Claude Managed Agents section

The four setup-only pages (`cloudflare-agents`, `graphite`, `nemo-agent-toolkit`, `pixeltable`) and the LangServe note on `langchain.mdx` are done (62dadc9). What remains:

- `anthropic.mdx` (headings: Install, Set up, What you get, Limits, Next): a "Claude Managed Agents" section before Limits (`m9-c` § Claude Managed Agents): its session spans come from Anthropic's servers and go to an OTLP endpoint a Console admin sets, so it is not run and cetra's endpoint must be publicly reachable.
- In Limits, from `spikes/m9-a-agent-frameworks.md` § spike F item 5: Anthropic's own SDKs (TypeScript `@anthropic-ai/sdk` 1.9.0, Python `anthropic` 2.9.0, the versions tested) emit `anthropic.messages.create` spans themselves (Python turns them off with `ANTHROPIC_OPEN_TELEMETRY=false`), so combining them with OpenInference's Anthropic instrumentor records each call twice. Before writing the sentence about the page's own setup, check in `sdks/python` and `sdks/typescript` whether cetra's `instrument=["anthropic"]` / `anthropic(Anthropic)` use OpenInference's instrumentor; write the double-count warning for the page's setup only if they do, and give the switch that avoids it.

**Done when:** `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs` pass. **Commit:** `Add a Claude Managed Agents section to the Anthropic page`.

## IN9.5 — Index and records

**Steps.**
1. Index (`index.mdx`; schema in `website/src/marketing/data/integrations.mjs` ~26–30: two language columns, How, tier; a captured row needs a `<LinkCard>` in its section's CardGrid, a setup-only row must have none; rows alphabetical within a section). In "Frameworks": eve (`— | ✓`, How `its \`otelIntegration\``, captured), Agent Spec (`✓ | —`, How `OpenInference's Agent Spec instrumentor`, captured), Atomic Agents (`✓ | —`, How `its \`AtomicAgentsInstrumentor\``, captured), TanStack AI (`— | ✓`, How `OpenInference's TanStack AI middleware`, captured), Flue (`— | ✓`, How `\`@flue/opentelemetry\``, captured), Graphite (`✓ | —`, How `grafi's OTLP/gRPC export`, setup only), NeMo Agent Toolkit (`✓ | —`, How `its OTLP exporter`, setup only), Cloudflare Agents (`— | ✓`, How `Cloudflare's OTLP destination`, setup only); in "Apps and tools" (where the Vector databases card is): Pixeltable (`✓ | —`, How `\`opentelemetry-instrumentation-pixeltable\``, setup only). LinkCard descriptions: one sentence each, in the style of the existing cards. EverOS (`✓ | —`, How `its built-in OTLP export`, captured) and Cognee (`✓ | —`, How `its built-in OTLP export`, setup only) in "Apps and tools". The last column is "Tested against". Sidebar entries in the same order.
2. `00-design.md`: the M9 milestone line "done"; the track's status line.
3. `mvp-decisions.md`: D-IN-31 after D-IN-30, copied from `00-design.md`.
4. CHANGELOG `Added` (the pages and captures) and `Changed` (the two rules).
5. Roadmap: "M9 implemented" and the track done; decisions D-IN-1–D-IN-31.
6. `just docs-gen` (its two cargo legs may run on ubuntu-dev).

**Done when:** `just check-docs` passes.

**Commit:** `List the long-tail framework pages and record D-IN-31`

## IN9.6 — EverOS (captured) and Cognee (setup only)

From `spikes/m9-a-agent-frameworks.md` § "Follow-up (spike E)" (EverOS) and `spikes/m9-b-rag-data.md` § "Follow-up (spike E)" (Cognee).

**EverOS:**
1. `tools/capture/everos/`: a uv tool project (`everos[otel]==1.7.0`, `package = false`, `exclude-newer = "7 days"`), installed by `fetch-tools.sh` in the shape of the LiteLLM Proxy block into `$tools_dir/everos/1.7.0` (binary `$CETRA_CAPTURE_TOOLS/everos/1.7.0/bin/everos`). Fetch `o200k_base.tiktoken` (§ Versions) with the existing `fetch` helper's checksum check into `$tools_dir/tiktoken/`, under the cache name.
2. Mock: embedding turns allow `dims` up to 64 today (`tools/capture/src/scenario.rs` ~661). Raise the upper bound to 4096 (EverOS needs 1024; Cognee's model 3072), update its two tests (~1139, ~1142) and the field's doc comment. `scenario.rs` joins this phase's edits.
3. Configuration: from `spikes/m9-a-agent-frameworks.md` § EverOS and its spike E follow-up, as environment variables of the spawned server (an explicit `env`, as IN9.2e): `EVEROS_ROOT` (a temporary folder; run `everos init` there first), `EVEROS_LLM__MODEL`, `EVEROS_LLM__API_KEY`, `EVEROS_LLM__BASE_URL` (the mock + `/v1`), `EVEROS_EMBEDDING__MODEL`, `EVEROS_EMBEDDING__BASE_URL`, `EVEROS_EMBEDDING__DIM=1024` (read `everos/config/default.toml` to confirm the names and that the vector store follows the dimension), `EVEROS_OBSERVABILITY__ENABLED=true`, `EVEROS_OBSERVABILITY__ENDPOINT` (the receiver's full `/v1/traces` URL), `EVEROS_OBSERVABILITY__CAPTURE_CONTENT=true`, `EVEROS_API__PORT=<free port>`, `TIKTOKEN_CACHE_DIR`. If a name differs in `default.toml`, use the file's and say so.
4. The extraction call asks for JSON matching EverOS's `Reply` model (spike E: "failed validation against Reply"): read that pydantic model in the installed package and write a fixed JSON answer that validates (the mock returns a turn's `message.text` verbatim and ignores `response_format`). If no fixed answer can validate, stop and ask.
5. Scenario `genai/everos-memory` (TypeScript, lifecycle as IN9.2e): spawn the server, poll `GET /health` (at most 60 s), `POST /api/v2/memory/add` (one message with a `sender_id`), `POST /api/v2/memory/flush`, `POST /api/v2/memory/search` (with a `user_id`), SIGINT/wait/SIGKILL in `try`/`finally`, then `writeMeta({}, { everos: "1.7.0" })` (EverOS is not an npm package). `instrumentation` `everos`; `libraries` `["everos"]`.
6. Turns: EverOS's requests carry no tools, so they take turns in arrival order; it also probes the embedding endpoint at startup. Run once with a generous script, read the order of requests from the mock's error log and the OTLP input, script the turns to match (chat turns with the JSON answer of step 4 where an extraction is asked, `openai.embeddings` turns with `dims` 1024), and set Expected from that run. Run `--check` twice; stop if the request order varies between runs.
7. Page `everos.mdx`: captured, in the shape of IN9.3's pages; Limits: install `everos[otel]` (without it nothing is exported); content off by default (`capture_content`); its model name carries a LiteLLM-style `openai/` prefix, which cetra does not price (the embedding model is priced).

**Cognee:** page `cognee.mdx`, setup only (D-IN-31 item 1: its run did not complete offline): `pip install 'cognee[tracing]'` (without the extra it logs that the exporter is not installed), `COGNEE_TRACING_ENABLED` and an OTLP/HTTP endpoint on port 4318 (Cognee picks HTTP only for endpoints matching its HTTP patterns, spike E); Limits: not run to completion; Cognee writes no token counts, so no cost; it also sends its own debug logs over OTLP, which cetra drops; content is on by default.

**Done when:** `just capture-tools` twice; the EverOS case passes `--check`; the IN9.3 page checks pass for both pages. **Commit:** `Capture EverOS and add the EverOS and Cognee pages`.

## After the last phase

`just check` (on ubuntu-dev for the Rust part, the capture check locally), a Sonnet code review, fixes, then a push to PR #242.
