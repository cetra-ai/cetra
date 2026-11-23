# Track IN, M2: providers — plan

A page for every model provider in the design's § Providers and the long tail it keeps, with the capture work their native SDKs need. M1 made OpenAI-compatible providers resolve from the endpoint host and priced their `vendor/model` rows. M2 adds:
- the hosts the provider research found;
- the Cohere and Ollama mock routes and an `httpx` real-host transport;
- five Python registry rows;
- seven captured cases (Groq, Mistral, Cohere, Together, Ollama, Azure OpenAI, Vertex AI);
- 36 pages (35 providers and aisuite);
- the regrouped integrations index, the sidebar, and `check-integrations.mjs`, which keeps the index's "Tested against" column true.

Implementer: Haiku, one agent per phase.

**Prerequisites.** Branch `claude/in-integrations` with M1 done (commit `75da9b9` or later). Node on `PATH`:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

Prefix every cargo or just command with:

```bash
CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target
```

**Read first.**
- `docs/plans/in-integrations/00-design.md`: § Provider from the endpoint, § Providers, the appendix, and D-IN-1, D-IN-2, D-IN-4, D-IN-7, D-IN-8, D-IN-11.
- `docs/plans/in-integrations/spikes/m2-native-sdks.md`: versions, injection parameters, wire formats with the exact JSON, and span attributes.
- `docs/plans/in-integrations/01-m1-provider-attribution.md`, for how M1 captured.
- `website/STYLE.md`.
- The existing pages `website/src/content/docs/docs/integrations/{openai,crewai,litellm}.mdx`.

## Amendments from the second review (override the phase text below)

1. **Expected lock changes.** In `tools/capture/py`, the first `uv lock` (IN2.2) also rewrites the `cetra-sdk` block (IN2.3 added extras), and adding the IN2.4 pins bumps `openinference-instrumentation` (0.1.75 → 0.1.77) and `openinference-semantic-conventions` (0.1.43 → 0.1.44). These changes are expected. Stop and report only for any other change to an existing pin.
2. **Existing cases must not drift.** IN2.4's Done when also runs `cargo run -q -p cetra-capture -- run --all --check`, so a new transitive version that changes an existing case is caught. If an existing case differs, stop and report the case and the diff.
3. **The OpenInference bullet on OpenAI-compatible pages (IN2.5, IN2.6, IN2.7).** Plain OpenInference in Python already names these hosts: `api.groq.com`, `api.x.ai`, `api.deepseek.com`, `api.fireworks.ai`, `api.together.xyz`, `api.together.ai`, `api.cerebras.ai`, `api.perplexity.ai`, `api.moonshot.cn`, `api.z.ai`, `api.minimax.io`, `*.openai.azure.com`. Pages for those providers (Groq and Together's "Through the OpenAI client" sections, xAI, DeepSeek, Fireworks AI, Cerebras, Perplexity, Moonshot, Z.ai, MiniMax) do not get the bullet "With plain OpenInference … shows as `openai`". Every other OpenAI-compatible page keeps it.
4. **`{api}` in scenario messages.** `scenario.rs` has no helper that gives an `Api`'s serde name. Add `fn name(self) -> &'static str` on `Api`, returning the serde names (`"openai.chat"`, …, `"cohere.chat"`, `"ollama.chat"`), with a unit test that `name()` equals the serde serialization for every variant, and use it in the new messages.
5. **Perplexity** base URL: `https://api.perplexity.ai`. Keep it. The `orchestrator/v1` path the research saw is a separate product, and `sonar` is served at the root.
6. **Azure TypeScript tab.** The imports block imports `OpenAI` and `AzureOpenAI` (`import OpenAI, { AzureOpenAI } from "openai";`), so the byte-compared `{/* example:integration-openai */}` block stays unchanged. The call block uses `new AzureOpenAI({ … })`.
7. **Vertex page Limits** add: "Through Vertex AI's OpenAI-compatible endpoint on a regional host (`<REGION>-aiplatform.googleapis.com`), the provider shows as `openai`; only the global `aiplatform.googleapis.com` host is mapped."
8. **Instrumentor classes in IN2.4.**
   - Azure uses `cetra_runtime(["openai"])`, which enables OpenInference's `OpenAIInstrumentor` itself, so no class is named in the program.
   - Vertex uses `from openinference.instrumentation.google_genai import GoogleGenAIInstrumentor`, then `GoogleGenAIInstrumentor().instrument(tracer_provider=provider)`.
   - The other five use the classes of IN2.3's table, from the same modules.

## What it is, and what it is not

When it is done:
- every provider below has a page;
- the index lists every integration page in sections, with a tier that a check verifies;
- the sidebar lists every page;
- `cetra-sdk[groq|mistralai|cohere|ollama|together]` installs the OpenInference instrumentor for that SDK;
- the Groq, Mistral, Cohere, Together, Ollama, Azure OpenAI and Vertex AI SDKs each have one captured case.

Not in M2:
- Gateways other than OpenRouter (M5).
- Streaming captures for Cohere and Ollama: their SSE and NDJSON formats are not mocked.
- `gcp.vertex_ai` as Vertex's provider. OpenInference writes `llm.provider=google` for both Google endpoints, so cetra shows `gcp.gen_ai`, which spec 02 prices with Gemini or Vertex rows (D-B1-11). The Vertex page says so (D-IN-21).
- Pages for TypeSafe (a decision-model API, not chat), Anyscale Endpoints (discontinued), Predibase (acquired, no public API), PrismML (no official API found) and the Meta Llama API (waitlisted preview, disputed host). They move to the design's § Not supported (D-IN-21).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-1 | One page per provider; the existing pages keep their scope. |
| D-IN-2 | Provider from the endpoint host; this plan adds hosts. |
| D-IN-4 | Mock routes by path suffix; new routes only for Cohere `/v2/chat` and Ollama `/api/chat`; Ollama's scrub host is `ollama.com`. |
| D-IN-7 | Registry rows for Groq, Mistral, Cohere, Ollama and Together. The import test skips a row whose extra is not installed, as for the existing rows; the dev group does not grow (D-IN-21 amends D-IN-7's "dev group"). |
| D-IN-8 | Four tiers. `check-integrations.mjs` reads the tier from the page: a `<Backing>` fixture whose `case.json` `origin` is `captured` → captured, `hand_written` → hand-written, `tests` → SDK test, no box → setup only. |
| D-IN-11 | Paid-account integrations are listed with setup-only parts. |
| D-IN-21 (new, recorded in the design by IN2.8) | M2's scope details. The five drops above. Vertex shows `gcp.gen_ai`. The registry import test keeps skipping when an extra is not installed. Pages for OpenAI-compatible providers without a capture are "setup only": the endpoint table's unit tests cover the host, not the provider's spans. D-IN-4's Ollama scrub allowance is not needed: the Ollama instrumentor writes no host, so nothing loopback reaches the scrub; Ollama's scrub host is `ollama.com`. |

## Versions

From the spike, at the 7-day rule (re-check with `uv lock`; report what was locked):

| Package | Where | Version |
|---|---|---|
| groq | capture project | 1.7.0 |
| mistralai | capture project | 3.7.0 |
| cohere | capture project | 7.3.1 |
| ollama | capture project | 0.6.3 |
| together | capture project | 3.1.1 |
| httpx | capture project, direct | the version already in `tools/capture/py/uv.lock` (0.28.1) |
| openinference-instrumentation-groq | capture project; SDK extra `>=` | 0.1.40 |
| openinference-instrumentation-mistralai | capture project; SDK extra `>=` | 2.3.5 |
| openinference-instrumentation-cohere | capture project; SDK extra `>=` | 0.1.22 |
| openinference-instrumentation-ollama | capture project; SDK extra `>=` | 0.1.20 |
| openinference-instrumentation-together | capture project; SDK extra `>=` | 0.1.20 |

`openai` (3.45.0) and `google-genai` (2.34.0) keep their capture pins.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN2.1 Hosts | `crates/otel/src/context.rs`, `docs/spec/02-ingestion.md` (§ Provider from the endpoint table) | — |
| IN2.2 Capture: Cohere and Ollama routes, `httpx` transport | `tools/capture/src/{scenario,mock}.rs`, `tools/capture/src/wire/{mod.rs, cohere.rs (new), ollama.rs (new)}`, `tools/capture/py/{pyproject.toml,uv.lock}`, `tools/capture/py/src/capture_py/harness.py` | — |
| IN2.3 Registry rows | `sdks/python/src/cetra/_instrument.py`, `sdks/python/pyproject.toml`, `sdks/python/uv.lock`, `sdks/python/tests/test_instrument.py`, `sdks/python/README.md`, `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/index.mdx`, `skills/cetra/references/python.md` (generated) | — |
| IN2.4 Captured cases | `tools/capture/py/scenarios/openinference/{groq,mistral,cohere,together,ollama,azure-openai,vertex-ai}-chat/`, `fixtures/otlp/openinference/<same>/`, `fixtures/otlp/prices.json` | IN2.1, IN2.2 |
| IN2.5 Pages: native providers | `website/src/content/docs/docs/integrations/{groq,mistral,cohere,together,ollama,azure-openai,vertex-ai}.mdx`, `google-genai.mdx` (one link), `skills/cetra/references/google-genai.md` (generated) | IN2.3, IN2.4 |
| IN2.6 Pages: OpenAI-compatible providers | the 18 pages of § Page data B (17 table rows and aisuite) | IN2.1 |
| IN2.7 Pages: long tail | the 11 pages of § Page data C | IN2.1 |
| IN2.8 Index, sidebar, check, docs | `website/src/content/docs/docs/integrations/index.mdx`, `website/astro.config.mjs`, `website/scripts/check-integrations.mjs` (new), `website/scripts/check-integrations.test.mjs` (new), `website/package.json`, `skills/cetra/references/integrations.md` (generated), `CHANGELOG.md`, `docs/roadmap.md`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md` | all |

The phases run one at a time in the track's worktree (the machine is shared). IN2.1, IN2.2 and IN2.3 have no file in common, so their order does not matter. Pages are not linked until IN2.8, so the link validator passes in between.

**Rules for every phase.**
- Run only the commands in "Done when". `cargo-nextest` is installed; `cargo test` with the same filters is also fine.
- Compile Rust once, right before the commit.
- Change no file outside the phase's Edit list.
- Commit once, with the given subject, and `git add` only the phase's files.
- If a step is unclear or contradicts the code, stop and report instead of improvising.

---

## IN2.1 — Hosts

**Goal:** the endpoint table knows the hosts the provider research found.

**Edit:** `crates/otel/src/context.rs`, `docs/spec/02-ingestion.md`.

**Steps.**

1. Add these rows to `ENDPOINT_PROVIDERS`. Put each one next to the existing rows of the same provider; place new providers before the gateway rows (`ai-gateway.vercel.sh` and after).

   ```rust
   ("api-inference.together.ai", "together_ai"),
   ("*.maas.aliyuncs.com", "dashscope"),
   ("api.cohere.com", "cohere"),
   ("api.cohere.ai", "cohere"),
   ("ollama.com", "ollama"),
   ("api.minimaxi.com", "minimax"),
   ("*.services.ai.azure.com", "azure.ai.openai"),
   ("*.cognitiveservices.azure.com", "azure.ai.openai"),
   ("aiplatform.googleapis.com", "gcp.vertex_ai"),
   ("*.gcp.databricks.com", "databricks"),
   ("*.api.baseten.co", "baseten"),
   ("ark.cn-beijing.volces.com", "volcengine"),
   ("ark.ap-southeast.bytepluses.com", "byteplus"),
   ("api.sarvam.ai", "sarvam"),
   ("qianfan.baidubce.com", "qianfan"),
   ("api.cometapi.com", "cometapi"),
   ("api.doubleword.ai", "doubleword"),
   ```

2. In the test `endpoint_providers_are_rule_12_fixed_points_and_priced_or_listed`, add `"byteplus"`, `"cometapi"`, `"doubleword"` and `"qianfan"` to `UNPRICED_ENDPOINT_PROVIDERS`, keeping it sorted. If another new name fails the test, stop and report it.

3. Add to `match_endpoint_uses_exact_hosts_and_label_suffixes`:
   - `x.maas.aliyuncs.com` → `dashscope`;
   - `us-central1-aiplatform.googleapis.com` → `None` (regional Vertex hosts are not listed; the OpenInference instrumentor names the provider itself);
   - `model-abc.api.baseten.co` → `baseten`;
   - `res.services.ai.azure.com` → `azure.ai.openai`.

4. Spec 02 § Provider from the endpoint: add the same rows to the host table, in the same order as in the code.

**Done when:**

```bash
cargo nextest run -p cetra-otel --lib context::tests
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo clippy -p cetra-otel --all-targets -- -D warnings
```

`cargo fmt -p cetra-otel` leaves no diff.

**Commit:** `Add provider hosts found by the M2 research`

---

## IN2.2 — Capture: Cohere and Ollama routes, `httpx` transport

**Goal:** the mock serves Cohere's v2 chat and Ollama's chat, and the Python harness can route plain-`httpx` SDKs to the mock under their real host.

**Edit:**
- `tools/capture/src/scenario.rs`, `tools/capture/src/mock.rs`;
- `tools/capture/src/wire/mod.rs`, and the new `tools/capture/src/wire/cohere.rs` and `tools/capture/src/wire/ollama.rs`;
- `tools/capture/py/pyproject.toml`, `tools/capture/py/uv.lock`, `tools/capture/py/src/capture_py/harness.py`.

**Steps.**

1. `scenario.rs`:
   - Add `Api` variants `CohereChat` (serde `"cohere.chat"`) and `OllamaChat` (serde `"ollama.chat"`).
   - Add `Provider` variants `Cohere` and `Ollama`, with `Provider::host()` returning `api.cohere.com` and `ollama.com` respectively.
   - Map `Api::CohereChat` to `Provider::Cohere` and `Api::OllamaChat` to `Provider::Ollama`.
   - Both are chat APIs: they need `message` and `finish`, like `openai.chat`. They reject `dims`.
   - `check_chat` rejects, for the two APIs and before the finish/tool-call rule:
     - tool calls: `turns[{i}].message.tool_calls are not supported by {api}`;
     - reasoning: `turns[{i}].message.reasoning is not supported by {api}`;
     - a finish other than `stop` or `length`: `turns[{i}].finish must be stop or length for {api}`.
   - `check_usage`: add an arm `Provider::Cohere | Provider::Ollama` that rejects a non-zero `cache_read`, `cache_write` or `reasoning` with `turns[{i}].usage.{field} is not reported by {api}` (`{field}` is the usage field name, `{api}` the API's serde name).

   Exhaustive matches the new variants reach, with their arms:
   - `mock.rs` `Route` enum gains `CohereChat` and `OllamaChat`.
   - `Route::api()` maps them to `Api::CohereChat` and `Api::OllamaChat`.
   - `read_facts`: `Route::CohereChat | Route::OllamaChat => (0, false, false, stream_field)`, exactly like `Route::Chat`, so the stream refusal fires.
   - `api_name`: `"cohere.chat"` and `"ollama.chat"`.
   - `render`: dispatch to `wire::cohere::chat` and `wire::ollama::chat`.
   - `request_id_header`: none for both (no request-id header).
   - `line_end`: `"\n"` for both (unreachable for non-streaming responses, but the match must be exhaustive).
   - The test helper `expected()` and the provider match in `each_route_matches_the_renderer` gain the two (no request-id header asserted).

   Add these cases to `providers_report_only_what_their_api_has`: a `cohere.chat` turn with `cache_read` fails, and an `ollama.chat` turn with a tool call fails.

2. `mock.rs` `classify`, before the existing suffix checks:
   - a path ending in `/v2/chat` is `Route::CohereChat`;
   - a path ending in `/api/chat` is `Route::OllamaChat`.

   Both are non-streaming in M2. A request body with `"stream": true` is answered like the existing streaming mismatch: the turn is consumed, an error is logged, and the status is 500.

3. `wire/cohere.rs`: `pub(crate) fn chat(turn: &Turn, ids: &Ids) -> Rendered` renders exactly this shape, returning the same `Rendered` variant that `wire::openai::chat` returns for a non-streaming response:

   ```json
   {"id":"<id>","finish_reason":"<COMPLETE|MAX_TOKENS>",
    "message":{"role":"assistant","content":[{"type":"text","text":"<turn.message.text>"}]},
    "usage":{"billed_units":{"input_tokens":<input>,"output_tokens":<output>},
             "tokens":{"input_tokens":<input>,"output_tokens":<output>}}}
   ```

   - `id` is `ids.cohere_chat()`. Add `Ids::cohere_chat()` to `wire/mod.rs`, returning `cohere-capture-{n}`: read `Ids::chat_completion()` and build it the same way, with the same counter.
   - `finish` `stop` → `COMPLETE`, `length` → `MAX_TOKENS`. Any other finish is rejected by `scenario.rs` for `cohere.chat`.

4. `wire/ollama.rs`: `pub(crate) fn chat(turn: &Turn, ids: &Ids) -> Rendered` renders exactly this shape, returning the same `Rendered` variant as `wire::openai::chat` for a non-streaming response (Ollama has no id; `ids` is taken for the shared dispatch and not used):

   ```json
   {"model":"<turn.model>","created_at":"2026-01-01T00:00:00Z",
    "message":{"role":"assistant","content":"<turn.message.text>"},
    "done":true,"done_reason":"<stop|length>","total_duration":1,"load_duration":1,
    "prompt_eval_count":<input>,"prompt_eval_duration":1,"eval_count":<output>,"eval_duration":1}
   ```

   Register both modules in `wire/mod.rs`, and dispatch the two routes to them in `mock.rs` next to the existing renderers.

5. `tools/capture/py/pyproject.toml`: add `"httpx==0.28.1"` (the version in the lock) to `dependencies`. Run `cd tools/capture/py && uv lock`. Stop and report if the lock changes any existing pin.

6. `harness.py`: add a twin of `RealHostTransport` on plain `httpx`:

   ```python
   class RealHostHttpxTransport(httpx.BaseTransport):
       """``RealHostTransport`` for SDKs built on ``httpx`` rather than ``httpx2`` (D-IN-3)."""
       # same body as RealHostTransport, with httpx.URL, httpx.HTTPTransport, httpx.Request, httpx.Response


   def real_host_httpx_client() -> httpx.Client:
       """An ``httpx.Client`` whose requests reach the mock under their real host."""
       return httpx.Client(transport=RealHostHttpxTransport())
   ```

   Import `httpx`. The module still passes ruff and mypy strict.

**Tests** (in the existing `mod tests`):
- `mock.rs` `each_route_matches_the_renderer`: add `POST /v2/chat` with a `cohere.chat` turn and `POST /api/chat` with an `ollama.chat` turn. Assert the JSON fields above.
- `mock.rs` `a_streaming_request_to_cohere_or_ollama_is_refused`: both routes with `"stream": true` give 500, log an error, and consume the turn.
- `scenario.rs`: as in step 1.

**Done when:**

```bash
cargo nextest run -p cetra-capture
cargo clippy -p cetra-capture --all-targets -- -D warnings
cd tools/capture/py && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy
```

`cargo fmt -p cetra-capture` leaves no diff.

**Commit:** `Mock Cohere and Ollama chat and route httpx SDKs to real hosts`

---

## IN2.3 — Registry rows

**Goal:** `cetra.init(instrument=["groq" | "mistralai" | "cohere" | "ollama" | "together"])` enables the matching OpenInference instrumentor.

**Edit:**
- `sdks/python/src/cetra/_instrument.py`, `sdks/python/pyproject.toml`, `sdks/python/uv.lock`, `sdks/python/tests/test_instrument.py`;
- `sdks/python/README.md`, `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/index.mdx`, `skills/cetra/references/python.md` (generated).

**Steps.**

1. Append five rows to `REGISTRY`, in this order, using the existing `Instrumentor(name, extra, module, cls)` shape:

   | name | extra | module | class |
   |---|---|---|---|
   | `groq` | `groq` | `openinference.instrumentation.groq` | `GroqInstrumentor` |
   | `mistralai` | `mistralai` | `openinference.instrumentation.mistralai` | `MistralAIInstrumentor` |
   | `cohere` | `cohere` | `openinference.instrumentation.cohere` | `CohereInstrumentor` |
   | `ollama` | `ollama` | `openinference.instrumentation.ollama` | `OllamaInstrumentor` |
   | `together` | `together` | `openinference.instrumentation.together` | `TogetherInstrumentor` |

   The class names are `GroqInstrumentor`, `MistralAIInstrumentor`, `CohereInstrumentor`, `OllamaInstrumentor` and `TogetherInstrumentor` (confirmed by the M2 spike).

2. `pyproject.toml` `[project.optional-dependencies]`:
   - one extra per row: `groq = ["openinference-instrumentation-groq>=0.1.40"]`, `mistralai = ["openinference-instrumentation-mistralai>=2.3.5"]`, `cohere = ["openinference-instrumentation-cohere>=0.1.22"]`, `ollama = ["openinference-instrumentation-ollama>=0.1.20"]`, `together = ["openinference-instrumentation-together>=0.1.20"]`;
   - the same five specifiers appended to `all`.

   Run `cd sdks/python && uv lock`.

3. `test_instrument.py`: the parametrized `test_other_registry_entries_import` takes its rows from `OTHER_ENTRIES`. If that list is built from the registry, nothing changes. If it is a literal, add the five names. Update the docstring count ("The eight other names") to the new number.

4. Spec 10: add the five rows to the § Instrumentors table, in the same order, and add `[groq]`, `[mistralai]`, `[cohere]`, `[ollama]`, `[together]` to the extras list in the heading of `## Python API` (line 178, before `[all]`).

5. `sdks/python/README.md`: add the same five rows to its extras table (`| \`instrument\` name | Extra |`), in the same order.

6. `reference/python/index.mdx`: add the five extras to the install table, in its format. Then run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` and commit what it rewrites.

**Done when:**

```bash
cd sdks/python && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy && uv run pytest tests/test_instrument.py tests/test_api_reference.py
cd website && node scripts/skill-references.mjs && node scripts/check-terms.mjs
```

All pass.

**Commit:** `Add Groq, Mistral, Cohere, Ollama and Together to the instrumentor registry`

---

## IN2.4 — Captured cases

**Goal:** one captured chat case per native SDK, in group `openinference`.

**Edit:** `tools/capture/py/{pyproject.toml,uv.lock}`, seven scenario directories, the seven fixture directories (written by `just capture` and `just fixtures-bless` only), `fixtures/otlp/prices.json`.

`[[tool.mypy.overrides]]` with `ignore_missing_imports = true` may be added to `tools/capture/py/pyproject.toml` for any new package that mypy reports as untyped; name each module added in the report.

**Steps.**

1. `tools/capture/py/pyproject.toml`: add exact pins for the ten packages of § Versions (SDKs and instrumentors). Run `uv lock`. Stop and report if the lock changes an existing pin, or if a version is refused by `exclude-newer`. In the latter case, report the newest version allowed and do not pick it yourself.

2. Seven scenarios. Each is `scenario.json` plus `main.py` (`from <module> import main` and the usual `if __name__ == "__main__": main()`) plus a module named `openinference_<slug with _>_chat.py`. Each sends one chat request, with the user message `What is the capital of France?`.
   - Every turn has `message.text` `"The capital of France is Paris."`, `finish` `stop`, `usage` `{input: 14, output: 8}`, and `host` as in the table.
   - `spec_refs`: `02 § Convention detection`, `02 § Model, provider, usage`, `02 § Cost computation`.
   - `instrumentation` is the instrumentor package. `libraries` is the SDK, the instrumentor and `opentelemetry-sdk`; for Azure it is `openai`, `openinference-instrumentation-openai` and `opentelemetry-sdk`.
   - Tracing setup: `provider = setup()`, then `<Instrumentor>().instrument(tracer_provider=provider)`. Azure uses `provider = cetra_runtime(["openai"])` instead, so the host stamp is pinned. Finish with `finish(provider)`.

   | Case | Client construction | Turn `api`, `model`, `host` |
   |---|---|---|
   | `groq-chat` | `Groq(api_key=os.environ["OPENAI_API_KEY"], http_client=real_host_httpx_client(), max_retries=0)` | `openai.chat`, `openai/gpt-oss-20b`, `api.groq.com` |
   | `mistral-chat` | `from mistralai.client import Mistral`; `Mistral(api_key=…, client=real_host_httpx_client())` | `openai.chat`, `mistral-small-latest`, `api.mistral.ai` |
   | `cohere-chat` | `cohere.ClientV2(api_key=…, httpx_client=real_host_httpx_client())` | `cohere.chat`, `command-a-03-2025`, `api.cohere.com` |
   | `together-chat` | `Together(api_key=…, http_client=real_host_httpx_client(), max_retries=0)` | `openai.chat`, `meta-llama/Llama-3.3-70B-Instruct-Turbo`, `api-inference.together.ai` |
   | `ollama-chat` | `ollama.Client(host="http://localhost:11434", transport=RealHostHttpxTransport())` | `ollama.chat`, `llama3.1`, (no `host`) |
   | `azure-openai-chat` | `AzureOpenAI(azure_endpoint="https://capture.openai.azure.com", api_version="2024-10-21", api_key=…, http_client=real_host_client(), max_retries=0)`, request `model="my-gpt4o-deployment"` | `openai.chat`, `gpt-4o-mini-2024-07-18`, `capture.openai.azure.com` |
   | `vertex-ai-chat` | `genai.Client(vertexai=True, project="capture", location="us-central1", credentials=google.oauth2.credentials.Credentials(token="capture-mock-key"), http_options=types.HttpOptions(httpx_client=real_host_httpx_client()))`, `client.models.generate_content(model="gemini-2.5-flash", contents=…)` | `gemini.generate_content`, `gemini-2.5-flash`, `us-central1-aiplatform.googleapis.com` |

   - All API keys come from `os.environ["OPENAI_API_KEY"]` (the harness sets it to the mock key for every program).
   - The Ollama case passes `api_key` to nothing: Ollama takes no key.
   - If google-genai 2.34.0 has no `HttpOptions(httpx_client=)`, stop and report. Do not change its pin.

   `notes` say how the provider resolves:
   - Groq, Mistral, Cohere, Together, Ollama: the instrumentor writes `llm.provider`.
   - Azure: `llm.provider=azure` and the stamped `server.address`.
   - Vertex: `llm.provider=google` gives `gcp.gen_ai`. That is the same as the Gemini API, and the span does not tell the two apart.

3. `fixtures/otlp/prices.json` (canonical format; `just fixtures-fmt`): add rows so that every case is priced:
   - `openai/gpt-oss-20b`, provider `groq`, input 0.075, output 0.3;
   - `mistral-small-latest`, provider `mistral_ai`, input 0.15, output 0.6;
   - `command-a-03-2025`, provider `cohere`, input 2.5, output 10.0;
   - `meta-llama/Llama-3.3-70B-Instruct-Turbo`, provider `together_ai`, input 1.04, output 1.04;
   - `llama3.1`, provider `ollama`, input 0.0, output 0.0;
   - always: pattern `gpt-4o-mini(-\d{4}-\d{2}-\d{2})?`, provider `azure.ai.openai`, input 0.15, output 0.6.

   For each row: `match_pattern` is `regex::escape` of the name (the Azure row uses the pattern above), `valid_from` is `1970-01-01T00:00:00.000000000Z`, and the other prices and `tiers` are null. The existing `gpt-4o-mini` row has provider `openai`, which does not price an `azure.ai.openai` span, so the Azure row is always added. Vertex is priced by the existing `gemini-2.5-flash` `gcp.gemini` row through the `gcp.gen_ai` rule.

4. Capture and bless:

   ```bash
   just capture openinference/groq-chat openinference/mistral-chat openinference/cohere-chat openinference/together-chat openinference/ollama-chat openinference/azure-openai-chat openinference/vertex-ai-chat
   just fixtures-bless
   ```

   `git status` must show no other changed fixture. Check each `expected.json`'s LLM span:
   - providers: `groq`, `mistral_ai`, `cohere`, `together_ai`, `ollama`, `azure.ai.openai`, `gcp.gen_ai`;
   - `cost_source` is `computed`;
   - `input_tokens` 14 and `output_tokens` 8.

   If any check fails, stop and report the excerpt.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run openinference/groq-chat openinference/mistral-chat openinference/cohere-chat openinference/together-chat openinference/ollama-chat openinference/azure-openai-chat openinference/vertex-ai-chat --check
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

All pass. The golden run includes `captured_llm_spans_are_priced_by_the_builtin_snapshot`. If it fails for a case because the built-in snapshot has no row for that model, stop and report.

**Commit:** `Capture Groq, Mistral, Cohere, Together, Ollama, Azure OpenAI and Vertex AI chat`

---

## Page template

Every new provider page follows this shape. Fill in the `<…>` values from the page data, and write in the voice of `website/STYLE.md`.

````mdx
---
title: <Title>
description: "<Description, at most 160 characters>"
---

import { Tabs, TabItem } from "@astrojs/starlight/components";
import Backing from "../../../../components/Backing.astro";

<One sentence: what the reader can trace, e.g. "Trace <Title> model calls with cetra through the OpenAI client.">

<Backing fixtures={["<group/case>", …]} />          ← only when the page data lists fixtures; omit the line and the import otherwise

## Install

<Tabs syncKey="lang">
  <TabItem label="Python">

```bash
pip install "cetra-sdk[<extra>]" <packages>
```

  </TabItem>
  <TabItem label="TypeScript">        ← only when the page data says TypeScript ✓

```bash
npm install @cetra-ai/sdk @arizeai/openinference-instrumentation-openai openai
```

  </TabItem>
</Tabs>

## Set up

<Tabs syncKey="lang">
  <TabItem label="Python">

{/* untested: calls <Title> and needs <ENV_VAR> */}
```python
<Python sample>
```

  </TabItem>
  <TabItem label="TypeScript">        ← only when TypeScript ✓; three blocks copied from openai.mdx

{/* untested: the imports of the example below */}
```ts
import { init } from "@cetra-ai/sdk";
import { openai } from "@cetra-ai/sdk/openai";
import OpenAI from "openai";
```

{/* example:integration-openai */}
```ts
init({ apiKey: process.env.CETRA_API_KEY, instrument: [openai(OpenAI)] });
```

{/* untested: calls <Title> and needs <ENV_VAR> */}
```ts
const client = new OpenAI({ baseURL: "<base URL>", apiKey: process.env.<ENV_VAR> });
await client.chat.completions.create({
  model: "<model>",
  messages: [{ role: "user", content: "What is the capital of France?" }],
});
```

  </TabItem>
</Tabs>

## What you get

<2–4 bullets: the span kind and name, the provider shown (`<provider>`), token counts, cost (priced by the built-in prices, or "no built-in price: add a custom model price in Settings → Model prices" when the page data says unpriced), the host in `server.address` when the provider comes from the endpoint.>

## Limits

<Bullets from the page data's "Limits", then always: "If a field is missing, look at the span's **Attributes** tab and compare it with [Span attributes](/docs/reference/attributes/).">

## Next

- [Traces](/docs/tracing/traces/): read the trace as a tree and a timeline.
- [Cost and tokens](/docs/tracing/cost-and-tokens/): how cost is computed from the token counts.
- [Custom model prices](/docs/tracing/cost-and-tokens/#custom-model-prices): price a model the built-in prices do not cover.
````

- Without a TypeScript column, the Install and Set up sections have one Python block each, with no `<Tabs>`.
- Any `<PLACEHOLDER>` outside a code block or inline code breaks MDX: keep placeholders inside backticks or code blocks.

The Python sample for an OpenAI-compatible provider is exactly:

```python
import os

import cetra
from openai import OpenAI

cetra.init(instrument=["openai"])

client = OpenAI(base_url="<base URL>", api_key=os.environ["<ENV_VAR>"])
response = client.chat.completions.create(
    model="<model>",
    messages=[{"role": "user", "content": "What is the capital of France?"}],
)
print(response.choices[0].message.content)

cetra.flush()
```

The install line for it is `pip install "cetra-sdk[openai]" openai`.

## IN2.5 — Pages: native providers

**Goal:** pages for the seven SDKs captured in IN2.4.

**Edit:** `website/src/content/docs/docs/integrations/{groq,mistral,cohere,together,ollama,azure-openai,vertex-ai}.mdx`, and `google-genai.mdx` (one sentence and link), and `skills/cetra/references/google-genai.md` (generated).

**Steps.** Write each page from the template and the table below.
- Each page shows its SDK's own client, as in IN2.4's scenario, without the capture transport, and `cetra.init(instrument=["<name>"])`.
- Groq and Together pages also have a short section `## Through the OpenAI client`, with the OpenAI-compatible sample (base URL in the table) and a sentence: "The provider comes from the endpoint host."
- In `google-genai.mdx`, add one sentence at the end of its intro: "For Vertex AI, see [Vertex AI](/docs/integrations/vertex-ai/)." Then run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` and commit what it rewrites.

| Slug | Title | Description | Extra and packages | Sample | Provider shown | Fixtures | TypeScript | Limits |
|---|---|---|---|---|---|---|---|---|
| groq | Groq | Trace Groq model calls with cetra from Python, through the Groq SDK or the OpenAI client. | `cetra-sdk[groq]` `groq` | `from groq import Groq`; `client = Groq(api_key=os.environ["GROQ_API_KEY"])`; model `openai/gpt-oss-20b`; OpenAI base URL `https://api.groq.com/openai/v1` | `groq` | `openinference/groq-chat` | — | Captured with groq 1.7.0 and openinference-instrumentation-groq 0.1.40. |
| mistral | Mistral AI | Trace Mistral AI model calls with cetra from Python, with the OpenInference Mistral instrumentation. | `cetra-sdk[mistralai]` `mistralai` | `from mistralai.client import Mistral`; `client = Mistral(api_key=os.environ["MISTRAL_API_KEY"])`; `client.chat.complete(model="mistral-small-latest", messages=[…])` | `mistral_ai` | `openinference/mistral-chat` | — | Captured with mistralai 3.7.0 and openinference-instrumentation-mistralai 2.3.5. |
| cohere | Cohere | Trace Cohere chat calls with cetra from Python, with the OpenInference Cohere instrumentation. | `cetra-sdk[cohere]` `cohere` | `import cohere`; `client = cohere.ClientV2(api_key=os.environ["COHERE_API_KEY"])`; `client.chat(model="command-a-03-2025", messages=[…])` | `cohere` | `openinference/cohere-chat` | — | Only non-streaming chat is captured; Cohere's response carries no model name, so the request model is shown. Captured with cohere 7.3.1 and openinference-instrumentation-cohere 0.1.22. |
| together | Together AI | Trace Together AI model calls with cetra from Python, through the Together SDK or the OpenAI client. | `cetra-sdk[together]` `together` | `from together import Together`; `client = Together(api_key=os.environ["TOGETHER_API_KEY"])`; model `meta-llama/Llama-3.3-70B-Instruct-Turbo`; OpenAI base URL `https://api.together.ai/v1` | `together_ai` | `openinference/together-chat` | — | Captured with together 3.1.1 and openinference-instrumentation-together 0.1.20. |
| ollama | Ollama | Trace models served by Ollama with cetra from Python, with the OpenInference Ollama instrumentation. | `cetra-sdk[ollama]` `ollama` | `import ollama`; `ollama.chat(model="llama3.1", messages=[…])` | `ollama` | `openinference/ollama-chat` | — | Only non-streaming chat is captured. Through the OpenAI client on `localhost`, the provider stays `openai`: cetra cannot tell a local server from another. Common Ollama models are in the built-in prices at 0; add a custom model price for others. Captured with ollama 0.6.3 and openinference-instrumentation-ollama 0.1.20. |
| azure-openai | Azure OpenAI | Trace Azure OpenAI model calls with cetra through the OpenAI SDK's Azure client. | `cetra-sdk[openai]` `openai` | `from openai import AzureOpenAI`; `client = AzureOpenAI(azure_endpoint=os.environ["AZURE_OPENAI_ENDPOINT"], api_key=os.environ["AZURE_OPENAI_API_KEY"], api_version="2024-10-21")`; `model="<DEPLOYMENT_NAME>"` | `azure.ai.openai` | `openinference/azure-openai-chat` | ✓ (TS client: `new AzureOpenAI({ endpoint: process.env.AZURE_OPENAI_ENDPOINT, apiKey: process.env.AZURE_OPENAI_API_KEY, apiVersion: "2024-10-21" })`, import `{ AzureOpenAI } from "openai"`) | The span shows the deployment's base model, which prices it. Calling Azure needs an Azure subscription; the capture ran against cetra's mock. |
| vertex-ai | Vertex AI | Trace Gemini on Vertex AI with cetra from Python, through the Google Gen AI SDK. | `cetra-sdk[google-genai]` `google-genai` | `from google import genai`; `client = genai.Client(vertexai=True, project=os.environ["GOOGLE_CLOUD_PROJECT"], location="us-central1")`; `client.models.generate_content(model="gemini-2.5-flash", contents="What is the capital of France?")` | `gcp.gen_ai` | `openinference/vertex-ai-chat` | — | The provider is `gcp.gen_ai`, the same as for the Gemini API: the spans do not say which Google endpoint served the call. Cost uses the Gemini or Vertex AI price. Calling Vertex AI needs a Google Cloud project and credentials; the capture ran against cetra's mock. |

**Done when:**

```bash
cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/skill-references.mjs
```

All pass. `pnpm --dir website exec astro build` is not required here; IN2.8 builds the site.

**Commit:** `Add pages for Groq, Mistral, Cohere, Together, Ollama, Azure OpenAI and Vertex AI`

---

## IN2.6 — Pages: OpenAI-compatible providers

**Goal:** pages for the providers used through the OpenAI client.

**Edit:** the 17 `.mdx` files of the table below and `aisuite.mdx`, 18 pages in all, in `website/src/content/docs/docs/integrations/`.

**Steps.** Write each page from the template, with the OpenAI-compatible Python sample.
- Pages with a TypeScript ✓ also get the TypeScript tab. OpenInference JS records the host, so the provider resolves there too.
- Every page whose fixtures are "—" adds this Limits bullet: "No spans of <Title> are captured in the repository. cetra maps the host `<host>` to the provider `<provider>`."
- "Unpriced" pages add: "The built-in prices have no <Title> rows, so cost is empty until you add a custom model price in Settings → Model prices."
- Every page adds: "With plain OpenInference or the official OpenAI instrumentation in Python, the provider shows as `openai`: they do not record the endpoint. Use `cetra.init(instrument=["openai"])` or OpenLLMetry."

| Slug | Title | Description | Base URL | Env var | Model | Provider (host) | Priced | Fixtures | TS | Extra Limits |
|---|---|---|---|---|---|---|---|---|---|---|
| openrouter | OpenRouter | Trace OpenRouter calls with cetra through the OpenAI client, with the provider and the model publisher's price. | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | `openai/gpt-4o-mini` | `openrouter` (`openrouter.ai`) | yes, at the model publisher's price | `openinference/openrouter-chat`, `openllmetry/openrouter-chat` | ✓ | Cost is the model publisher's price for models named `vendor/model` from OpenAI, Anthropic, Google, Mistral, DeepSeek, Cohere or xAI; OpenRouter's fees are not included. Other models have no built-in price. |
| xai | xAI | Trace xAI Grok model calls with cetra through the OpenAI client. | `https://api.x.ai/v1` | `XAI_API_KEY` | `grok-4.3` | `x_ai` (`api.x.ai`) | yes | — | ✓ | — |
| deepseek | DeepSeek | Trace DeepSeek model calls with cetra through the OpenAI client. | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` | `deepseek-chat` | `deepseek` (`api.deepseek.com`) | yes | — | ✓ | — |
| fireworks | Fireworks AI | Trace Fireworks AI model calls with cetra through the OpenAI client. | `https://api.fireworks.ai/inference/v1` | `FIREWORKS_API_KEY` | `accounts/fireworks/models/llama-v3p1-8b-instruct` | `fireworks_ai` (`api.fireworks.ai`) | yes | — | ✓ | — |
| novita | Novita AI | Trace Novita AI model calls with cetra through the OpenAI client. | `https://api.novita.ai/openai` | `NOVITA_API_KEY` | `deepseek/deepseek-v3.2` | `novita` (`api.novita.ai`) | yes | `openinference/novita-chat`, `openllmetry/novita-chat` | ✓ | — |
| qwen | Qwen (Alibaba Cloud Model Studio) | Trace Qwen models on Alibaba Cloud Model Studio (DashScope) with cetra through the OpenAI client. | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | `DASHSCOPE_API_KEY` | `qwen-plus` | `dashscope` (`dashscope-intl.aliyuncs.com`, `*.maas.aliyuncs.com`) | yes | — | ✓ | Model Studio keys are bound to a region; use that region's base URL. |
| cerebras | Cerebras | Trace Cerebras model calls with cetra through the OpenAI client. | `https://api.cerebras.ai/v1` | `CEREBRAS_API_KEY` | `llama-3.3-70b` | `cerebras` (`api.cerebras.ai`) | yes | — | ✓ | — |
| perplexity | Perplexity | Trace Perplexity model calls with cetra through the OpenAI client. | `https://api.perplexity.ai` | `PERPLEXITY_API_KEY` | `sonar` | `perplexity` (`api.perplexity.ai`) | yes | — | ✓ | Search results and citations stay in the raw response; cetra records the messages and token counts. |
| moonshot | Moonshot AI (Kimi) | Trace Moonshot AI Kimi model calls with cetra through the OpenAI client. | `https://api.moonshot.ai/v1` | `MOONSHOT_API_KEY` | `kimi-k2.5` | `moonshot` (`api.moonshot.ai`, `api.moonshot.cn`) | yes | — | ✓ | — |
| z-ai | Z.ai (GLM) | Trace Z.ai GLM model calls with cetra through the OpenAI client. | `https://api.z.ai/api/paas/v4/` | `ZAI_API_KEY` | `glm-4.6` | `zai` (`api.z.ai`, `open.bigmodel.cn`) | yes | — | ✓ | — |
| sambanova | SambaNova | Trace SambaNova Cloud model calls with cetra through the OpenAI client. | `https://api.sambanova.ai/v1` | `SAMBANOVA_API_KEY` | `Meta-Llama-3.3-70B-Instruct` | `sambanova` (`api.sambanova.ai`) | yes | — | ✓ | — |
| nebius | Nebius AI Studio | Trace Nebius AI Studio model calls with cetra through the OpenAI client. | `https://api.tokenfactory.nebius.com/v1/` | `NEBIUS_API_KEY` | `deepseek-ai/DeepSeek-R1-0528` | `nebius` (`api.tokenfactory.nebius.com`, `api.studio.nebius.com`) | yes | — | ✓ | — |
| hugging-face | Hugging Face Inference Providers | Trace Hugging Face Inference Providers calls with cetra through the OpenAI client. | `https://router.huggingface.co/v1` | `HF_TOKEN` | `deepseek-ai/DeepSeek-R1:fastest` | `huggingface` (`router.huggingface.co`) | no (unpriced) | — | ✓ | The router picks a provider for each call; the span names Hugging Face, not the provider that served it. |
| cloudflare-workers-ai | Cloudflare Workers AI | Trace Cloudflare Workers AI model calls with cetra through the OpenAI client. | `https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1` | `CLOUDFLARE_API_KEY` | `@cf/meta/llama-3.1-8b-instruct-fp8` | `cloudflare` (`api.cloudflare.com`) | yes | — | ✓ | Cloudflare AI Gateway is a different product with its own page later. |
| databricks | Databricks | Trace Databricks Foundation Model API calls with cetra through the OpenAI client. | `https://<WORKSPACE_HOST>/serving-endpoints` | `DATABRICKS_TOKEN` | `databricks-claude-sonnet-4-5` | `databricks` (`*.cloud.databricks.com`, `*.azuredatabricks.net`, `*.gcp.databricks.com`) | yes, for model names (not endpoint ids) | — | ✓ | Calling Databricks needs a workspace; this page is not tested against one. |
| vllm | vLLM | Trace models served by vLLM's OpenAI-compatible server with cetra. | `http://localhost:8000/v1` | `VLLM_API_KEY` | `<MODEL_NAME>` | `openai` (local server: not mapped) | no (unpriced) | — | ✓ | cetra cannot tell a vLLM server from OpenAI by its host, so the provider shows as `openai` and the built-in prices do not apply. Add a custom model price for the model you serve. |
| watsonx | IBM watsonx.ai | Trace IBM watsonx.ai model calls with cetra from Python, with the OpenLLMetry watsonx instrumentation. | — (sample below) | `WATSONX_APIKEY` | `ibm/granite-3-8b-instruct` | `ibm.watsonx.ai` | no (unpriced) | — | — | Setup only: calling watsonx needs an IBM Cloud account, and no spans are captured. |

The aisuite page (`aisuite.mdx`) is not the OpenAI-compatible template:
- Title `aisuite`; description "Trace aisuite calls with cetra through the instrumentation of each provider SDK it wraps."
- Install line: `pip install "cetra-sdk[openai,anthropic]" "aisuite[openai,anthropic]"`.
- Set up: one Python block, marked `{/* untested: calls OpenAI and needs OPENAI_API_KEY */}`:

  ```python
  import aisuite as ai
  import cetra

  cetra.init(instrument=["openai", "anthropic"])

  client = ai.Client()
  response = client.chat.completions.create(
      model="openai:gpt-4o-mini",
      messages=[{"role": "user", "content": "What is the capital of France?"}],
  )
  print(response.choices[0].message.content)

  cetra.flush()
  ```

- No TypeScript, no `<Backing>` (setup only).
- Limits: "aisuite calls each provider's own SDK, so the spans come from that SDK's instrumentation; a provider without an instrumentation in `cetra-sdk` is not traced." Then the usual closing Limits bullet.

The watsonx page does not use the OpenAI-compatible sample:
- Install line: `pip install ibm-watsonx-ai opentelemetry-instrumentation-watsonx opentelemetry-exporter-otlp-proto-http`.
- Set up: one `{/* untested: needs an IBM Cloud account */}` Python block. It creates a `TracerProvider` exporting to cetra, exactly like the block in `google-adk.mdx` (copy it), then calls `WatsonxInstrumentor().instrument(tracer_provider=provider)` (`from opentelemetry.instrumentation.watsonx import WatsonxInstrumentor`), with the comment `# Call watsonx.ai with ibm_watsonx_ai as usual.` and `provider.shutdown()`.

For a TypeScript ✓ page whose base URL has a `<…>` placeholder, keep the placeholder in the TypeScript sample too.

**Done when:**

```bash
cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs
```

All pass.

**Commit:** `Add pages for providers used through the OpenAI client`

---

## IN2.7 — Pages: long tail

**Goal:** pages for the long-tail OpenAI-compatible providers.

**Edit:** the 11 `.mdx` files of the table below. The Sarvam sample differs from the template, see below the table.

**Steps.** Same rules as IN2.6, with every page TypeScript ✓ and no fixtures.

| Slug | Title | Description | Base URL | Env var | Model | Provider (host) | Priced | Extra Limits |
|---|---|---|---|---|---|---|---|---|
| byteplus | BytePlus ModelArk | Trace BytePlus ModelArk model calls with cetra through the OpenAI client. | `https://ark.ap-southeast.bytepluses.com/api/v3` | `ARK_API_KEY` | `<ENDPOINT_ID>` | `byteplus` | no | Model ids come from your ModelArk console. |
| volcengine | Volcengine Ark | Trace Volcengine Ark (Doubao) model calls with cetra through the OpenAI client. | `https://ark.cn-beijing.volces.com/api/v3` | `ARK_API_KEY` | `<ENDPOINT_ID>` | `volcengine` | yes, for model names (not endpoint ids) | Model ids come from your Ark console. |
| baseten | Baseten | Trace Baseten Model APIs calls with cetra through the OpenAI client. | `https://inference.baseten.co/v1` | `BASETEN_API_KEY` | `deepseek-ai/DeepSeek-V3.1` | `baseten` (`inference.baseten.co`, `*.api.baseten.co`) | yes | Dedicated deployments use their own `model-<ID>.api.baseten.co` host, which also maps to Baseten. |
| deepinfra | DeepInfra | Trace DeepInfra model calls with cetra through the OpenAI client. | `https://api.deepinfra.com/v1/openai` | `DEEPINFRA_TOKEN` | `meta-llama/Meta-Llama-3.1-8B-Instruct` | `deepinfra` | yes | — |
| hyperbolic | Hyperbolic | Trace Hyperbolic model calls with cetra through the OpenAI client. | `https://api.hyperbolic.xyz/v1` | `HYPERBOLIC_API_KEY` | `deepseek-ai/DeepSeek-R1` | `hyperbolic` | yes | — |
| minimax | MiniMax | Trace MiniMax model calls with cetra through the OpenAI client. | `https://api.minimax.io/v1` | `MINIMAX_API_KEY` | `MiniMax-M2` | `minimax` (`api.minimax.io`, `api.minimaxi.com`) | yes | — |
| nvidia-nim | NVIDIA NIM | Trace NVIDIA NIM model calls with cetra through the OpenAI client. | `https://integrate.api.nvidia.com/v1` | `NVIDIA_API_KEY` | `meta/llama-3.3-70b-instruct` | `nvidia_nim` (`integrate.api.nvidia.com`) | no | A self-hosted NIM container on your own host shows the provider `openai`, as any local server does. |
| sarvam | Sarvam AI | Trace Sarvam AI model calls with cetra through the OpenAI client. | `https://api.sarvam.ai/v1` | `SARVAM_API_KEY` | `sarvam-105b` | `sarvam` | no | Sarvam authenticates with the `api-subscription-key` header. |
| qianfan | Baidu Qianfan | Trace Baidu Qianfan (ERNIE) model calls with cetra through the OpenAI client. | `https://qianfan.baidubce.com/v2` | `QIANFAN_API_KEY` | `ernie-4.5-turbo-128k` | `qianfan` | no | — |
| cometapi | CometAPI | Trace CometAPI calls with cetra through the OpenAI client. | `https://api.cometapi.com/v1` | `COMETAPI_KEY` | `gpt-4o-mini` | `cometapi` | no | CometAPI resells many publishers' models; the span names CometAPI, and the built-in prices do not apply. |
| doubleword | Doubleword | Trace Doubleword inference calls with cetra through the OpenAI client. | `https://api.doubleword.ai/v1` | `DOUBLEWORD_API_KEY` | `<MODEL_NAME>` | `doubleword` | no | — |

The Sarvam sample passes `default_headers={"api-subscription-key": os.environ["SARVAM_API_KEY"]}` to `OpenAI(...)`, together with `api_key=os.environ["SARVAM_API_KEY"]` (both in Python and TypeScript, where the option is `defaultHeaders`).

**Done when:** as IN2.6.

**Commit:** `Add pages for long-tail providers used through the OpenAI client`

---

## IN2.8 — Index, sidebar, check, docs

**Goal:** the index and the sidebar list every integration page by section, a check keeps the index's tier column true, and the decisions and records are current.

**Edit:**
- `website/src/content/docs/docs/integrations/index.mdx`, `website/astro.config.mjs`;
- `website/scripts/check-integrations.mjs` (new), `website/scripts/check-integrations.test.mjs` (new), `website/package.json`;
- `skills/cetra/references/integrations.md` (generated);
- `CHANGELOG.md`, `docs/roadmap.md`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md`.

**Steps.**

1. **Index.** Keep the frontmatter and the first sentence. Replace the single table and the card grids with one section per group, each a `##` heading followed by a table with the existing columns `| Integration | Python | TypeScript | How | Tested against |`:
   - Sections, in this order: Providers, Frameworks, Coding agents, Instrumentation libraries, Migrating.
   - Rows inside a section are alphabetical by title.
   - Every existing row keeps its cells. Existing rows go to:
     - Providers: OpenAI, Anthropic, Google Gen AI, Amazon Bedrock, LiteLLM, plus the 35 new provider pages;
     - Frameworks: LangChain and LangGraph, LlamaIndex, Vercel AI SDK, OpenAI Agents SDK, CrewAI, DSPy, Google ADK, plus the new aisuite page;
     - Coding agents: Claude Code, Codex CLI, Gemini CLI, GitHub Copilot;
     - Instrumentation libraries: OpenInference, OpenLLMetry (Traceloop), OpenTelemetry GenAI;
     - Migrating: From Langfuse, From Laminar.
   - Rows sort by title, case-insensitive.
   - Remove the now-unused `CardGrid, LinkCard` import.
   - For a new row, "How" is `instrument=["<name>"]` for native SDK pages, and "OpenAI client with the provider's base URL" for OpenAI-compatible pages ("OpenLLMetry watsonx instrumentation" for watsonx, "the instrumentation of each provider SDK" for aisuite).
   - "Tested against" is `captured` for pages with a `<Backing>` of captured fixtures, and `setup only` otherwise. Python is ✓ for all new rows. TypeScript is ✓ where the page data says so, else —.

   After the tables, replace the definitions paragraph with four sentences, one per tier:
   - "Captured" means the repository holds spans recorded from the real library, and a test checks how cetra reads them.
   - "Hand-written" means the spans in the repository were written from the vendor's attribute reference.
   - "SDK test" means an SDK test checks the helper, and the repository holds no recorded spans.
   - "Setup only" means the page shows the setup; nothing in the repository records this integration's spans.

   Keep the existing sentence about —, including the one about ✓ for TypeScript, and the final "Anything else…" sentence.

2. **Sidebar** (`astro.config.mjs`, the Integrations topic). Make the Providers group's items every provider page (the five existing plus the 35 new provider pages), alphabetical by label, and set `collapsed: true` on that group. Put `aisuite` in the Frameworks group, alphabetically. Each label equals its page's `title`. The other groups stay as they are.

3. **`check-integrations.mjs`.** Same structure as `check-backing.mjs`: a pure `checkIndex(indexText, pages, caseOrigin)` returning `path:line: message` strings, and `main(root, fixturesRoot)`. `pages` maps a slug to its `.mdx` text; `caseOrigin(group/case)` returns the `origin` of that case's `case.json`, or `null`.
   - Parse every table row of the index whose first cell is a link `[<title>](/docs/integrations/<slug>/)`. Take its last cell as the tier text.
   - The tier text is one or more parts separated by `, `. Each part is `<tier>` or `<tier> (<language>)`, and `<tier>` is one of `captured`, `hand-written`, `SDK test`, `setup only`. Anything else is a finding.
   - For each part, read the page's `<Backing>` box (the same regex as `check-backing.mjs`):
     - `captured` needs at least one fixture with `caseOrigin` `captured`;
     - `hand-written` needs at least one fixture with origin `hand_written`;
     - `SDK test` needs a non-empty `tests`;
     - `setup only` needs no `<Backing>` box at all.
   - Every `.mdx` file in the integrations directory except `index.mdx` has exactly one row; every row's slug has a file.

   `main` reads `src/content/docs/docs/integrations/` and `../fixtures/otlp/<case>/case.json`. It prints the findings and returns 0 or 1. The guarded entry runs `main("src/content/docs/docs/integrations", "../fixtures/otlp")`.

4. **`check-integrations.test.mjs`** (`node:test`, the `check-backing.test.mjs` style). Cases:
   - a captured row with a captured fixture passes;
   - a captured row whose fixture is hand-written fails;
   - a setup-only row whose page has a box fails;
   - an `SDK test` row without `tests` fails;
   - `captured (Python), SDK test (TypeScript)` with both passes;
   - an unknown tier fails;
   - a page without a row fails;
   - a row without a page fails.

5. **`package.json`.** Insert `node scripts/check-integrations.mjs &&` right after `node scripts/check-backing.mjs &&` in `check:content`. Format with `cd website && ../node_modules/.bin/biome check --write scripts/`.

6. **Generated references.** Run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` and commit what it rewrites (the index is a skill reference).

7. **Design and decisions.**
   - `00-design.md`: add D-IN-21 to the decisions table, with the text of this plan's row. Amend D-IN-7's cell: "The import test skips a row whose extra is not installed, as for the existing rows (D-IN-21)." Move TypeSafe, Anyscale, Predibase, PrismML and Meta Llama API from the appendix into § Not supported, with their reasons from § What it is not. Add the IN2.1 hosts to § Provider from the endpoint's table. Amend D-IN-4: Ollama's scrub host is `ollama.com`, and its scrub allowance for `localhost:11434` is dropped (the Ollama instrumentor writes no host).
   - `mvp-decisions.md` § Track IN: add D-IN-21, and amend D-IN-7 and D-IN-4 the same way.

8. **`CHANGELOG.md` `[Unreleased]` `### Added`.** "Integration pages for 35 model providers and aisuite, with captured spans for Groq, Mistral AI, Cohere, Together AI, Ollama, Azure OpenAI and Vertex AI; `cetra-sdk` extras `groq`, `mistralai`, `cohere`, `ollama` and `together`."

9. **`docs/roadmap.md`**, IN row:
   - Plan cell: add "M2: [`02-m2-providers.md`](plans/in-integrations/02-m2-providers.md)".
   - Status: "**M1, M2 implemented** (…; M2: 36 integration pages (35 providers and aisuite), seven captured SDKs, Cohere and Ollama mock routes, `check-integrations`)."
   - Decisions cell: `D-IN-1`–`D-IN-21`.

**Done when:**

```bash
cd website && node --test scripts/ && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/check-integrations.mjs && node scripts/skill-references.mjs
just check-docs
```

Both pass. `just check-docs` builds the site and validates every link.

**Commit:** `Regroup the integrations index and check its tiers`

---

## After the last phase

The main session:
- runs `just check` once;
- runs a Sonnet code review of M2, and has its findings fixed by a Haiku agent;
- pushes to PR #242.
