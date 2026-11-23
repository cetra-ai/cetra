# Track IN, M5: gateways — plan

Pages for every gateway in the design's § Gateways that a spike kept, and captured cases for the ones that run locally:
- LiteLLM Proxy (pip);
- Bifrost, Envoy AI Gateway (`aigw` with Envoy), and the Temporal dev server that M3 left setup-only. These three are binaries the user approved on 2026-11-23.

The normalizer learns:
- LiteLLM's cost keys;
- embedding operation names;
- Bifrost's root span kind, flattened tool calls and cost split;
- LiteLLM's tool-call output.

The capture tool learns to run pinned binaries.

Implementer: the GLM 5.3 agent, one agent per phase. Conventions as in M4:
- Node on `PATH`;
- `CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target`;
- `git commit -m … -- <paths>`;
- `touch tools/capture/src/main.rs` before the first capture;
- never `--run-ignored`.

**Read first:**
- `docs/plans/in-integrations/00-design.md`: § Gateways, D-IN-2, D-IN-13, D-IN-16.
- `spikes/m5-gateways.md`: LiteLLM's real run, per-gateway verdicts, proposed rules 1–8.
- `spikes/m5-binaries.md`: Bifrost, aigw and Temporal runs; rules B1–B3; § Capture-tool changes; § Page notes; the download table (URL, size, SHA-256).
- `docs/spec/02-ingestion.md`.
- `tools/capture/src/{mock,run,scenario}.rs` and `tools/capture/py/src/capture_py/harness.py`.

## What it is, and what it is not

When it is done:
- LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal have captured cases;
- every gateway the spikes kept has a page;
- the index has a Gateways section;
- `just capture-tools` fetches the pinned binaries, with their checksums.

Not in M5:
- **Kong.** It needs Docker, which this machine lacks. Its page is setup only (the user, 2026-11-24).
- **Azure API Management.** It has no OTLP trace export. It goes to § Not supported.
- **Pydantic AI Gateway.** It exports to Logfire only. The Pydantic AI page gets one sentence; no page of its own.
- **Databricks AI Gateway.** One sentence on the existing Databricks page.
- **Gravitee.** Its spans are plain HTTP spans. It goes to § Not supported.
- **Cloudflare AI Gateway's OTLP export.** No recorded export exists. The page documents the OpenAI-client route and lists the export's gaps.
- **The Vercel AI Gateway trace drain.** It is SaaS, needs a paid plan, and carries no content. The page documents the OpenAI-client route and mentions the drain, setup only.
- **New hosts for Anannas, OrcaRouter or TrueFoundry.** The spike advises against them.
- **Envoy's request model, rule E1.** The page notes that the model column shows the response model.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-13 | Gateway pricing. LiteLLM Proxy writes the upstream provider, so it needs no `litellm_proxy` entry. |
| D-IN-16 | Gateway-reported cost: `gen_ai.cost.total_cost`, `gen_ai.cost.input_cost` and `gen_ai.cost.output_cost` (LiteLLM, captured), plus Bifrost's `bifrost.cost.input` and `bifrost.cost.output`. |
| D-IN-25 (new, recorded by IN5.5) | M5's decisions, listed below. |

D-IN-25 records these decisions:
1. **Pinned binaries.** The user approved Temporal CLI 1.10.1, Bifrost 2.5.1, aigw 1.2.1 and Envoy 1.39.1 (2026-11-23 and 2026-11-24). They come from the vendors' GitHub releases, are pinned by SHA-256, and are fetched by `just capture-tools` into `target/capture-tools/`. Envoy is fetched from `envoyproxy/envoy`, never by `aigw` at run time.
2. **Kong** is setup only, because there is no Docker.
3. **Bifrost's server root span** (scope `bifrost`, no `gen_ai.operation.name`) is `chain`.
4. **A `tool_calls` element with `name`/`args` and no `function` object** is a tool call (Bifrost).
5. **LiteLLM's empty assistant output with `gen_ai.completion.0.function_call.*`** becomes a tool-call output.
6. **`gen_ai.operation.name` `embedding` and `aembedding`** map to `embedding`.
7. **Gateway pages warn that an instrumented app behind a traced gateway counts each call twice,** because both honour `traceparent`.
8. **Azure API Management and Gravitee** go to § Not supported; **Pydantic AI Gateway and Databricks AI Gateway** are merged into their providers' pages.

## Versions

| Item | Version | Source |
|---|---|---|
| `litellm[proxy]` (capture project) | 1.109.1, the version the spike ran (check the 7-day rule with `uv lock`) | PyPI |
| `temporalio`, `temporalio-openai-agents[openinference]` | 1.36.1, 2.0.0 | PyPI |
| Temporal CLI | 1.10.1 | `temporalio/cli` GitHub release |
| Bifrost | 2.5.1 | `maximhq/bifrost` GitHub release |
| aigw | 1.2.1 | `envoyproxy/ai-gateway` GitHub release (now redirected to `theagentrouter/agent-router`) |
| Envoy | 1.39.1 | `envoyproxy/envoy` GitHub release |

Take the exact URLs, sizes and SHA-256s from `spikes/m5-binaries.md`.

`temporalio-openai-agents` 2.0.0 was uploaded 2026-11-17T21:07Z. If `uv lock` refuses it, the run is before 2026-11-24T21:07Z: wait, or add an `exclude-newer-package` line for that one package, as the project already does for google-adk. Report which you did.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN5.1 Capture tool for binaries | `tools/capture/src/{mock,run}.rs`, `tools/capture/py/src/capture_py/harness.py`, `tools/capture/fetch-tools.sh` (new), `justfile`, `AGENTS.md` (§ Required tools) | — |
| IN5.2 Normalizer rules | `crates/otel/src/{kind,cost}.rs`, `crates/otel/src/conventions/{genai,mod}.rs`, `crates/otel/src/convert/openai.rs`, `docs/spec/02-ingestion.md` | — |
| IN5.3 Captures | `tools/capture/py/{pyproject.toml,uv.lock}`, scenario directories, fixture directories | IN5.1, IN5.2 |
| IN5.4 Pages | gateway pages, and one sentence each in `temporal.mdx`, `databricks.mdx`, `pydantic-ai.mdx` | IN5.3 |
| IN5.5 Index, sidebar, records | as IN4.10 | IN5.4 |

IN5.1 and IN5.2 touch disjoint files. They run one after the other in this worktree, in either order.

---

## IN5.1 — Capture tool for binaries

**Goal:** a scenario can start a pinned binary and record it, as `spikes/m5-binaries.md` § Capture-tool changes describes.

**Steps.**
1. **`mock.rs`:** a `GET` whose path ends in `/models` is answered with `{"object":"list","data":[]}`. It takes no turn and logs no error, as `HEAD /api/hello` does today. Unit test `get_models_is_answered_without_a_turn`.
2. **`run.rs`:**
   - Spawn the program in its own process group (`std::os::unix::process::CommandExt::process_group(0)`, behind `cfg(unix)`). On timeout and on drop, kill the group (`libc::killpg`, or the `nix` crate only if it is already a dependency; otherwise send `kill -TERM -<pgid>` via `std::process::Command`), then the child.
   - Add `CETRA_CAPTURE_TOOLS=<repo>/target/capture-tools` to `program_env`, and update `program_env_drops_parent_secrets_and_applies_scenario_env`.
   - Add a test that a program spawning a long-lived child is cleaned up on timeout. Use `sh -c "sleep 60 & sleep 60"` with a short test timeout if the module allows injecting one; otherwise test the group-kill helper on its own.
3. **`harness.py`:** add `finish_tool(provider: TracerProvider | None, versions: dict[str, str]) -> None`, writing the meta file with the given versions. With `provider=None` there is no flush. Read the existing `finish` and reuse its meta-writing code.
4. **`tools/capture/fetch-tools.sh`** (bash, `set -euo pipefail`):
   - A table of name, version, URL, size and SHA-256 for the four binaries, taken from the spike.
   - For each: if `target/capture-tools/<name>/<version>/` is complete, skip it. Otherwise download into a temporary file with `curl -fsSL`, check the SHA-256 with `sha256sum -c` and fail on mismatch, then extract into that directory.
   - Linux x86_64 only. Any other platform prints that the binaries are not available and exits 0.
5. **`justfile`:** a `capture-tools` recipe running the script; `capture` and `check-capture` depend on it.
6. **`AGENTS.md` § Required tools:** "for captures, `just capture-tools` downloads Temporal CLI, Bifrost, aigw and Envoy into `target/capture-tools` (pinned, checksummed)".

**Done when:**

```bash
cargo nextest run -p cetra-capture
cargo clippy -p cetra-capture --all-targets -- -D warnings
just capture-tools
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

`just capture-tools` succeeds twice in a row (the second run downloads nothing). `bash -n tools/capture/fetch-tools.sh` passes.

**Commit:** `Let capture scenarios run pinned gateway binaries`

## IN5.2 — Normalizer rules

**Goal:** the rules of D-IN-16 and D-IN-25 items 3–6, each with unit tests. Spec 02 is edited in the same commit.

**Steps.**
1. **`cost.rs`, reported cost** (spec 02 § Cost computation step 1):
   - `TOTAL_KEYS` gains `gen_ai.cost.total_cost`.
   - `INPUT_KEYS` gains `gen_ai.cost.input_cost` and `bifrost.cost.input`.
   - `OUTPUT_KEYS` gains `gen_ai.cost.output_cost` and `bifrost.cost.output`.
   - Each key goes last in its list.
2. **`kind.rs`, `gen_ai.operation.name` row:** `embedding` and `aembedding` map to `embedding`.
3. **`kind.rs`, Bifrost** (scope-gated, the pattern of the Agent Framework row): when `scope_name == "bifrost"` and there is no `gen_ai.operation.name`, the kind is `chain` when the span has a model key (`gen_ai.request.model` or `gen_ai.response.model`). Otherwise the row decides nothing, so the existing rules still apply.
4. **The OpenAI chat converter** (`convert/openai.rs`, used for `gen_ai.input.messages` and `gen_ai.output.messages`):
   - A `tool_calls` element with `name` and no `function` object becomes `ToolCall { id, name, arguments }`, from `id`, `name` and `args`. `args` is parsed the way `function.arguments` is.
   - `content: ""` next to tool calls adds no empty text part.
5. **LiteLLM tool-call output** (`conventions/genai.rs`): when the resolved output is an assistant message with empty `parts` and the span has `gen_ai.completion.0.function_call.name`, the output becomes one assistant message with one `ToolCall { name, arguments }` built from `gen_ai.completion.0.function_call.{name,arguments}`, and those keys are consumed.

Read `spikes/m5-gateways.md` § LiteLLM for the exact key spelling and value shapes. If they differ from this step, stop and report.

**Tests:**
- each new cost key, and the margin case: `total_cost` differs from the price table, and reported cost wins;
- `embedding` and `aembedding`;
- the Bifrost root span with and without the scope, and a Bifrost `chat` span that stays `llm`;
- the flattened tool call, and an empty content string next to tool calls;
- the LiteLLM tool output, and output left alone when `parts` is not empty.

**Done when:** `cargo nextest run -p cetra-otel`, clippy and fmt pass, with no `expected.json` changed.

**Commit:** `Read gateway cost keys, embedding operations and Bifrost spans`

## IN5.3 — Captures

**Goal:** captured cases for LiteLLM Proxy, Bifrost, aigw and Temporal.

**Steps.**
1. **Capture project:** add `litellm[proxy]`, `temporalio` and `temporalio-openai-agents[openinference]` at § Versions, then `uv lock`.
   - Report every changed existing pin. If one moves, run the whole `run --all --check`, and stop if an existing case drifts.
   - Set `LITELLM_LOCAL_MODEL_COST_MAP=True` in every LiteLLM scenario's `main.py`.
2. **Scenarios**, each written from the spike's run scripts (`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m5/litellm/` and `…/m5bin/`), adapted to the harness:
   - the mock at `CETRA_CAPTURE_MOCK_URL`;
   - OTLP to `CETRA_CAPTURE_OTLP_ENDPOINT`, with an `Authorization: Bearer capture-mock-key` header. A literal space, not `%20`: the spike found LiteLLM sends `%20` literally;
   - binaries from `CETRA_CAPTURE_TOOLS`;
   - free ports from `bind(0)`;
   - the gateway stopped with SIGINT in `try/finally` before `finish_tool`.

   | Case | Group | Content |
   |---|---|---|
   | `litellm-proxy-chat` | `genai` | the proxy with two models (`gpt-4o-mini` direct and an alias to an upstream with a provider prefix), one chat each, plus one tool-call turn |
   | `litellm-proxy-embedding` | `genai` | one embedding |
   | `bifrost-chat` | `genai` | one chat with a tool call, then the answer. Use `span_names` to drop the 24 `plugin.*` spans. Use the spike's offline config: `file://` pricing URLs, `mcp_library_sync_interval: 0`, `client.enforce_auth_on_inference: false` |
   | `envoy-ai-gateway-chat` | `openinference` | `aigw run` with the pre-placed Envoy (`ENVOY_VERSION=1.39.1`, a short `AIGW_RUNTIME_DIR` under `/tmp`), one chat |
   | `temporal-openai-agents` | `openinference` | `WorkflowEnvironment.start_local(dev_server_existing_path=<CETRA_CAPTURE_TOOLS>/temporal/1.10.1/temporal)`, one workflow whose agent calls one tool through the Responses API (`openai.responses` turns: a tool call, then text), with `OpenTelemetryPlugin` and `OpenAIAgentsPlugin(use_otel_instrumentation=True)` |

   - Binary scenarios call `finish_tool` with their binary versions (`{"bifrost": "2.5.1"}`, `{"aigw": "1.2.1", "envoy": "1.39.1"}`).
   - Temporal calls `finish` plus the CLI version, through `finish_tool` or an extra entry.
   - Nondeterminism goes in `volatile_attributes` and `timing_attributes`, as the spike lists.
3. Capture, bless, and check each `expected.json`:
   - LiteLLM: the provider is the upstream's; cost is `reported` with the `gen_ai.cost.*` values, including the aliased model; the embedding span is `embedding`; the tool turn's output is a tool call.
   - Bifrost: exactly one `llm` and one `chain`; reported cost; the tool call is parsed.
   - aigw: one `llm`, provider from `llm.system`, cost computed.
   - Temporal: the OpenAI Agents spans as in `openinference/openai-agents-tool`, and Temporal's spans as `span`.

   Stop and report every deviation.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run --all --check
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

All pass.

**Commit:** `Capture LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal`

## IN5.4 — Pages

**Goal:** the gateway pages, written from the spikes' per-gateway sections and § Page notes.

**Steps.** Pages: `litellm-proxy`, `bifrost`, `envoy-ai-gateway` (captured); `kong`, `portkey`, `truefoundry`, `vercel-ai-gateway`, `helicone`, `cloudflare-ai-gateway`, `orcarouter`, `anannas` (setup only).
- OpenAI-client pages (Portkey, Vercel AI Gateway, Helicone, Cloudflare, OrcaRouter, Anannas) use the M2 OpenAI-compatible template:
  - provider shown: the gateway's name where the host is mapped, else `openai`;
  - pricing: the publisher's price through D-IN-13 where mapped, else none;
  - a short "Its own OTLP export" section where one exists (TrueFoundry, Vercel drain, Cloudflare), marked setup only, with what is known to be missing.
- Every gateway page's Limits carries the double-count warning (D-IN-25 item 7).
- `temporal.mdx` becomes captured: a `<Backing>` box, What you get from the fixture, and the setup as built (`temporalio-openai-agents` 2.0.0).
- One sentence each in `databricks.mdx` (the AI Gateway logs to Unity Catalog, not OTLP) and `pydantic-ai.mdx` (Pydantic AI Gateway exports to Logfire only).

**Done when:** as IN3.3.

**Commit:** `Add gateway pages`

## IN5.5 — Index, sidebar, records

**Goal:** as IN4.10.

**Steps.**
1. **Index and sidebar:**
   - add a **Gateways** section after Providers, with rows, tiers and cards for tested rows, and a sidebar group;
   - OpenRouter stays under Providers;
   - Temporal's row becomes `captured`, with a card.
2. **Design:**
   - D-IN-25 (this plan's text);
   - § Gateways rows as built;
   - Vercel AI Gateway's export noted;
   - § Not supported gains Azure API Management and Gravitee, with their reasons.

   The same changes go into `mvp-decisions.md`.
3. **Changelog and roadmap:**
   - CHANGELOG `Added`: gateway pages, the LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal captures, `just capture-tools`.
   - CHANGELOG `Changed`: LiteLLM's and Bifrost's reported cost read; Bifrost's root span is a chain; embedding operations.
   - Roadmap: the M5 plan link, status "M1–M5 implemented", decisions `D-IN-1`–`D-IN-25`.

**Done when:** as IN3.4.

**Commit:** `List the gateway pages and record D-IN-25`

## After the last phase

The main session:
- runs `just check` (touch the crates that embed `CARGO_MANIFEST_DIR` first);
- runs a Sonnet review;
- has a GLM agent fix the findings;
- pushes to PR #242.
