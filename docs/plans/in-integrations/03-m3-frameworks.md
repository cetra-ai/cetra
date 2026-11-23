# Track IN, M3: OpenInference frameworks — plan

Pages, registry rows and captured cases for the frameworks traced through OpenInference: Instructor, Agno, smolagents, Haystack, BeeAI (Python), Guardrails AI and DeepAgents. Temporal gets a setup-only page, and LangGraph and the Claude Agent SDK get their own pages backed by existing fixtures. The M3 spike (`spikes/m3-frameworks.md`) ran every framework offline under its instrumentor and found no normalizer gap, so M3 changes no Rust normalizer code.

Implementer: Haiku, one agent per phase.

**Prerequisites:** branch `claude/in-integrations` with M2 done (commit `d8b510b` or later). Use the conventions of `02-m2-providers.md`:
- Node on `PATH`: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.
- `CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target` on cargo and just commands.
- Commit with `git commit -m … -- <paths>`.

**Read first:**
- `docs/plans/in-integrations/00-design.md`: § Agent frameworks, D-IN-1, D-IN-7, D-IN-8, D-IN-17.
- `docs/plans/in-integrations/spikes/m3-frameworks.md`: this plan relies on it for each framework's client injection, turns, span shapes and pitfalls. Read the section of your framework in full.
- `docs/plans/in-integrations/02-m2-providers.md`: the page template, and how IN2.4 captured.
- The spike programs in `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m3c/` (`t_instructor.py`, `t_agno.py`, `t_smol.py`, `t_haystack.py`, `t_bee.py`, `t_guard2.py`, `t_deep.py`): working code to start each scenario from.
- `website/STYLE.md`.

## What it is, and what it is not

When it is done:
- `cetra.init(instrument=["instructor" | "agno" | "smolagents" | "haystack" | "beeai"])` enables that instrumentor;
- each of those frameworks, plus Guardrails AI and DeepAgents, has a captured case in group `openinference`, and a page;
- Temporal, LangGraph and the Claude Agent SDK have pages;
- the index, the sidebar and the decisions are current.

Not in M3:
- **BeeAI TypeScript.** The upstream instrumentor supports only `beeai-framework` `>=0.1.9 <0.1.14`, a 2025 line, and exports no span on the current 0.1.32. It goes to § Not supported (D-IN-22).
- **A Guardrails registry row.** The OpenInference Guardrails instrumentor only supports guardrails-ai below 0.5.1. Current Guardrails emits its own spans (D-IN-22).
- **A Temporal capture.** It needs a pinned Temporal dev-server binary, which the test environment downloads at runtime. The page is setup only, and the roadmap notes the wait (D-IN-22).
- **Haystack `Agent` tracing.** The instrumentor traces pipelines only; the page says agents are not traced as agents.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-1 | One page per integration. `langchain` is retitled "LangChain" and links `langgraph`; `claude-code` links `claude-agent-sdk`. |
| D-IN-7 | Registry rows. M3 adds `instructor`, `agno`, `smolagents`, `haystack`, `beeai`; `guardrails` is dropped (D-IN-22). |
| D-IN-8 | Tiers. The seven captured frameworks are `captured`; Temporal is `setup only`. LangGraph and the Claude Agent SDK are `captured`, through existing fixtures. |
| D-IN-22 (new, recorded by IN3.4) | M3's scope changes from the spike. No `guardrails` registry row: Guardrails' native spans plus `instrument=["openai"]`. BeeAI TypeScript is not supported upstream. Temporal is setup only until a pinned dev-server binary exists. The Temporal route is `temporalio-openai-agents`' `OpenAIAgentsPlugin(use_otel_instrumentation=True)`, not "a tracing interceptor". Agno, smolagents, Haystack and BeeAI pages tell readers not to enable `openai` as well, because a second `llm` span per call would count tokens and cost twice. Instructor and Guardrails need `openai`. |

## Versions

At the 7-day rule, from the spike. The capture project uses exact pins, and the SDK extras use `>=` the instrumentor version.

| Package | Version |
|---|---|
| instructor | 1.17.0 |
| openinference-instrumentation-instructor | 0.1.35 |
| agno | 3.2.0 |
| openinference-instrumentation-agno | 1.2.1 |
| smolagents (with `openai` extra) | 1.26.0 |
| openinference-instrumentation-smolagents | 0.1.50 |
| haystack-ai | 3.5.0 |
| openinference-instrumentation-haystack | 0.1.53 |
| beeai-framework | 0.1.85 (the version the capture project's litellm pin allows) |
| openinference-instrumentation-beeai | 0.1.36 |
| guardrails-ai | 0.11.0 |
| deepagents | 0.7.32 (the version langchain 1.4.8 allows) |

Capture project overrides: `[tool.uv] override-dependencies = ["jiter>=0.16", "openai==3.45.0"]`. Instructor caps `jiter<0.15` and guardrails-ai caps `openai<3`; the spike checked both work.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN3.1 Registry rows | `sdks/python/src/cetra/_instrument.py`, `sdks/python/pyproject.toml`, `sdks/python/uv.lock`, `sdks/python/tests/test_instrument.py`, `sdks/python/README.md`, `docs/spec/10-sdk.md`, `website/src/content/docs/docs/reference/python/index.mdx`, `skills/cetra/references/python.md` (generated) | — |
| IN3.2 Captured cases | `tools/capture/py/{pyproject.toml,uv.lock}`, seven scenario directories, seven fixture directories | — |
| IN3.3 Pages | ten new pages, `langchain.mdx`, `claude-code.mdx`, generated skill references | IN3.1, IN3.2 |
| IN3.4 Index, sidebar, records | `index.mdx`, `astro.config.mjs`, generated skill references, `CHANGELOG.md`, `docs/roadmap.md`, `00-design.md`, `docs/mvp-decisions.md` | IN3.3 |

IN3.1 and IN3.2 share no file and can run in parallel. **Rules for every phase:** as in M2. Run only "Done when"; change only the Edit list; commit once with the given subject; stop and report when a step cannot be done as written.

---

## IN3.1 — Registry rows

**Goal:** five new `instrument=` names.

**Steps.** Same as IN2.3, with these rows appended in this order:

| name | extra | module | class | extra specifier |
|---|---|---|---|---|
| `instructor` | `instructor` | `openinference.instrumentation.instructor` | `InstructorInstrumentor` | `openinference-instrumentation-instructor>=0.1.35` |
| `agno` | `agno` | `openinference.instrumentation.agno` | `AgnoInstrumentor` | `openinference-instrumentation-agno>=1.2.1` |
| `smolagents` | `smolagents` | `openinference.instrumentation.smolagents` | `SmolagentsInstrumentor` | `openinference-instrumentation-smolagents>=0.1.50` |
| `haystack` | `haystack` | `openinference.instrumentation.haystack` | `HaystackInstrumentor` | `openinference-instrumentation-haystack>=0.1.53` |
| `beeai` | `beeai` | `openinference.instrumentation.beeai` | `BeeAIInstrumentor` | `openinference-instrumentation-beeai>=0.1.36` |

- Add the five specifiers to `all`.
- The `instructor` and `beeai` instrumentors do not support Python 3.15 (`<3.14` and `<3.15`). Give those two extras a marker, `; python_version < "3.14"` and `; python_version < "3.15"` respectively, both in their own extra and in `all`, so `uv lock` resolves for every supported Python. The import test then skips them there, because the module is absent.
- Update the import test docstring's count to eighteen.
- Add the five rows to spec 10's table and to its `## Python API` heading, to `sdks/python/README.md`'s extras table, and to the website reference install table. Regenerate the skill references.

**Done when:** as IN2.3's.

**Commit:** `Add Instructor, Agno, smolagents, Haystack and BeeAI to the instrumentor registry`

---

## IN3.2 — Captured cases

**Goal:** one captured case per framework, in group `openinference`.

**Steps.**

1. `tools/capture/py/pyproject.toml`: add the exact pins of § Versions, their instrumentors, and `[tool.uv] override-dependencies = ["jiter>=0.16", "openai==3.45.0"]`. If an override key already exists, append to it. Run `uv lock`.
   - Stop and report if an existing pin changes. The exception: transitive bumps that `uv lock` needs for these packages may change. List each one in the report, with its old and new version.
   - Stop and report if the lock fails.

2. Seven scenarios. Each is `scenario.json`, `main.py` and a module named `openinference_<case with _>.py`. Start each one from the spike program, and route the model client to the mock the way the existing framework scenario `openinference/langgraph-agent` does: `base_url=mock_url("/v1")`. These are framework cases, not attribution cases: no real host and no turn `host`.
   - Every turn uses model `gpt-4o-mini-2024-07-18`.
   - `spec_refs`: `02 § Convention detection`, `02 § Mapping tables / Span kind`, `02 § Model, provider, usage`.
   - The `notes` describe the span tree the case records, and anything the page must warn about.

   | Case | Instrumentation setup | Turns |
   |---|---|---|
   | `instructor-structured-output` | `InstructorInstrumentor` and OpenInference's `OpenAIInstrumentor`, both on the harness provider | 1: `openai.chat` tool call `User {"name": "Ada", "age": 36}`, id `call_user_1`, finish `tool_calls`, usage 30/9 |
   | `agno-agent-tool` | `AgnoInstrumentor` only; `os.environ["AGNO_TELEMETRY"] = "false"` before the import | 2: tool call `get_weather {"city": "Paris"}` id `call_weather_1`, finish `tool_calls`, usage 82/17; then text `It is sunny in Paris.`, finish `stop`, usage 131/12 |
   | `smolagents-code-agent` | `SmolagentsInstrumentor` only; `CodeAgent` with one `@tool get_weather` | 2: the two text turns with `<code>` blocks given in the spike's smolagents section, usage 82/17 and 131/12 |
   | `haystack-pipeline` | `HaystackInstrumentor` only; `os.environ["HAYSTACK_TELEMETRY_ENABLED"] = "False"` before the import; pipeline `ChatPromptBuilder` → `OpenAIChatGenerator` | 1: text `Paris is the capital of France.`, finish `stop`, usage 25/9 |
   | `beeai-agent` | `BeeAIInstrumentor` only; `RequirementAgent` with one tool `get_weather` | 2: the two JSON text turns of the spike's BeeAI section, finish `stop`, usage 82/17 and 131/12 |
   | `guardrails-guard` | `cetra_runtime(["openai"])` is NOT used. Instead: harness `setup()` provider, set as the global provider (Guardrails reads the global one), plus OpenInference's `OpenAIInstrumentor`. In `main.py`, before importing guardrails, point `HOME` at a fresh temp directory that holds `.guardrailsrc` with `enable_metrics=false`. A local `@register_validator` validator, as in the spike's `t_guard2.py`; `OPENAI_API_KEY` comes from the environment, never as a keyword | 1: text `Paris is the capital of France.`, finish `stop`, usage 25/9 |
   | `deepagents-agent` | OpenInference's `LangChainInstrumentor`; `create_deep_agent(model=ChatOpenAI(...), tools=[get_weather])`; pin framework ids as `langgraph-agent`'s scenario does, extended for any id the capture shows changing | 2: tool call `get_weather {"city": "Paris"}`, finish `tool_calls`, usage 82/17; then text `It is 18 °C and sunny in Paris.`, usage 131/12 |

   `libraries` lists the framework, its instrumentor (and `openinference-instrumentation-openai` where it is used, plus `langchain-openai` for DeepAgents), and `opentelemetry-sdk`. `instrumentation` is the framework's instrumentor package, except Guardrails, whose is `guardrails-ai`.

3. Randomness:
   - Random ids that make a recapture differ go in `volatile_attributes` (see `scenario.rs` for its semantics), or are pinned in the program, as in `langgraph-agent`.
   - If an id is inside a JSON string that `volatile_attributes` cannot reach (the spike names Guardrails' `callId` inside `output.value`), limit the case with `span_names` to the spans whose content is stable.
   - If `--check` still differs between two captures, stop and report. In that case the framework's page becomes setup only (IN3.3 handles that).

4. Capture and bless the seven cases (`just capture openinference/<case> …`, then `just fixtures-bless`). Check each `expected.json`:
   - kinds as the spike predicts;
   - every `llm` span has its tokens and `cost_source` `computed` or `reported` (BeeAI reports its cost);
   - `git status` shows no change to other fixtures.

   Prices: `gpt-4o-mini` is already in `fixtures/otlp/prices.json` and the snapshot, so `prices.json` does not change. If a case needs a row, stop and report.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run openinference/instructor-structured-output openinference/agno-agent-tool openinference/smolagents-code-agent openinference/haystack-pipeline openinference/beeai-agent openinference/guardrails-guard openinference/deepagents-agent --check
cargo run -q -p cetra-capture -- run --all --check
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

All pass. The `--all` run proves no existing case drifted with the new transitive versions. Report the cases that were dropped under step 3, if any.

**Commit:** `Capture Instructor, Agno, smolagents, Haystack, BeeAI, Guardrails AI and DeepAgents runs`

---

## IN3.3 — Pages

**Goal:** ten pages, and the two existing pages linked.

**Edit:**
- `website/src/content/docs/docs/integrations/{instructor,agno,smolagents,haystack,beeai,guardrails,deepagents,temporal,langgraph,claude-agent-sdk}.mdx`;
- `langchain.mdx`, `claude-code.mdx`;
- `skills/cetra/references/*.md` that the regeneration rewrites.

**Steps.** Write each page in the shape of M2's page template: intro sentence, `<Backing>` when captured, Install, Set up, What you get, Limits, Next. Python only, with every sample marked `{/* untested: … */}`.
- **What you get** lists the span tree the captured case shows, in cetra's kinds; read each `expected.json`.
- **Limits** names the captured library versions from `case.json`, and the warnings below.
- **Next** links Traces, Cost and tokens, and [Online rules](/docs/evaluation/online-rules/).
- A framework whose case was dropped in IN3.2 step 3 gets no `<Backing>`, and its Limits say no spans are captured.

| Slug | Title | Description | Install | Set up | Warnings for Limits |
|---|---|---|---|---|---|
| instructor | Instructor | Trace Instructor structured outputs with cetra from Python, with the model call, its tokens and cost. | `pip install "cetra-sdk[instructor,openai]" instructor` | `cetra.init(instrument=["instructor", "openai"])`; `client = instructor.from_openai(OpenAI())`; `client.chat.completions.create(model="gpt-4o-mini", response_model=User, messages=[…])` with a pydantic `User(name: str, age: int)` | Enable `openai` too: Instructor's own span carries no token counts. The `instructor` extra needs Python 3.13 or earlier. |
| agno | Agno | Trace Agno agents with cetra from Python: agent runs, model calls and tool calls. | `pip install "cetra-sdk[agno]" agno openai` | `cetra.init(instrument=["agno"])`; `Agent(model=OpenAIChat(id="gpt-4o-mini"), tools=[get_weather]).run("What is the weather in Paris?")` | Do not also enable `openai`: each model call would be recorded twice and counted twice. Agno sends its own telemetry unless `AGNO_TELEMETRY=false`. |
| smolagents | smolagents | Trace Hugging Face smolagents runs with cetra from Python: agent steps, model calls and tools. | `pip install "cetra-sdk[smolagents]" "smolagents[openai]"` | `cetra.init(instrument=["smolagents"])`; `CodeAgent(tools=[get_weather], model=OpenAIServerModel(model_id="gpt-4o-mini")).run("What is the weather in Paris?")` | Do not also enable `openai`. The agent span repeats the run's total tokens; cetra counts tokens on model calls only, so nothing is counted twice. |
| haystack | Haystack | Trace Haystack pipelines with cetra from Python: components and model calls. | `pip install "cetra-sdk[haystack]" haystack-ai` | `cetra.init(instrument=["haystack"])`; a `Pipeline` with `ChatPromptBuilder` and `OpenAIChatGenerator(model="gpt-4o-mini")`, connected and run | Do not also enable `openai`. The `Agent` component is not traced as an agent: its model calls appear as separate traces. Haystack sends its own telemetry unless `HAYSTACK_TELEMETRY_ENABLED=False`. |
| beeai | BeeAI Framework | Trace BeeAI Framework agents with cetra from Python: agent runs, model calls and tools. | `pip install "cetra-sdk[beeai]" beeai-framework` | `cetra.init(instrument=["beeai"])`; `RequirementAgent(llm=OpenAIChatModel("gpt-4o-mini"), tools=[get_weather])` run with `await agent.run("What is the weather in Paris?")` inside `asyncio.run` | Cost is the one BeeAI reports (through LiteLLM), not cetra's price table. TypeScript is not supported: OpenInference's BeeAI instrumentation only covers 2025 versions of the TypeScript framework. Needs Python 3.14 or earlier. |
| guardrails | Guardrails AI | Trace Guardrails AI guards and validators with cetra from Python, with the model call they check. | `pip install "cetra-sdk[openai]" guardrails-ai` | `cetra.init(instrument=["openai"])` before creating the guard; `guard = Guard().use(<VALIDATOR>)`; `guard(model="gpt-4o-mini", messages=[…])` | Pass the API key through `OPENAI_API_KEY`: a key passed as an argument ends up in the recorded parameters. Guardrails sends usage metrics unless `enable_metrics` is off in `~/.guardrailsrc`. Guardrails writes its own spans; the OpenInference Guardrails instrumentation does not support current versions. |
| deepagents | Deep Agents | Trace LangChain Deep Agents with cetra from Python, through the OpenInference LangChain instrumentation. | `pip install "cetra-sdk[langchain]" deepagents langchain-openai` | `cetra.init(instrument=["langchain"])`; `create_deep_agent(model=ChatOpenAI(model="gpt-4o-mini"), tools=[get_weather]).invoke({"messages": [{"role": "user", "content": "What is the weather in Paris?"}]})` | Middleware steps appear as `agent` spans. |
| temporal | Temporal | Trace OpenAI Agents SDK workflows on Temporal with cetra from Python, through Temporal's OpenAI Agents plugin. | `pip install cetra-sdk "temporalio-openai-agents[openinference]" temporalio` | One untested block: `from temporalio.contrib.opentelemetry import create_tracer_provider` (check the import path in the installed package, as the spike describes); `provider = create_tracer_provider()`; `trace.set_tracer_provider(provider)`; then `cetra.init()` (cetra adds its exporter to the provider you set); then `Client.connect("localhost:7233", plugins=[OpenAIAgentsPlugin(use_otel_instrumentation=True)])` | Setup only: no spans of Temporal are captured in the repository. Do not also pass `instrument=["openai-agents"]`: the plugin instruments the OpenAI Agents SDK itself. Temporal's own workflow and activity spans appear as plain spans. |
| langgraph | LangGraph | Trace LangGraph graphs with cetra from Python and TypeScript, through the OpenInference LangChain instrumentation. | as `langchain.mdx` | as `langchain.mdx`'s LangGraph example (`instrument=["langchain"]`, `create_agent` or `create_react_agent`) | Graph nodes are `chain` spans named after the node; LangGraph's own keys stay in the span's metadata. Backing: `openinference/langgraph-agent` (plus `openllmetry/langgraph-agent` if `langchain.mdx` cites it). |
| claude-agent-sdk | Claude Agent SDK | Trace Claude Agent SDK runs with cetra from Python and TypeScript, through Claude Code's OpenTelemetry spans. | `pip install claude-agent-sdk`; `npm install @anthropic-ai/claude-agent-sdk` | the settings and environment of `claude-code.mdx` § Claude Agent SDK, for both languages | The application's own span (the parent of the Claude Code turn) is whatever your application's tracing produces. Backing: `claude_code/bash-tool`. Link [Claude Code](/docs/integrations/claude-code/) for the span shapes. |

- `langchain.mdx`: retitle it "LangChain" (frontmatter `title`), and add one sentence to its intro: "For LangGraph graphs and Deep Agents, see [LangGraph](/docs/integrations/langgraph/) and [Deep Agents](/docs/integrations/deepagents/)."
- `claude-code.mdx` § Claude Agent SDK: add one sentence: "See also [Claude Agent SDK](/docs/integrations/claude-agent-sdk/)."
- Run `cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs` and commit what it rewrites.

**Done when:**

```bash
cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/skill-references.mjs
```

All pass.

**Commit:** `Add pages for M3's frameworks, LangGraph and the Claude Agent SDK`

---

## IN3.4 — Index, sidebar, records

**Goal:** the new pages are listed, and the decisions and records are current.

**Steps.**

1. **Index.** Add the ten rows to the Frameworks section, alphabetical by title, case-insensitive.
   - Python ✓; TypeScript ✓ for LangGraph and the Claude Agent SDK only.
   - "How": `instrument=["<name>"]`; `instrument=["instructor", "openai"]` for Instructor; `instrument=["openai"]` plus Guardrails' own spans for Guardrails; `instrument=["langchain"]` for DeepAgents and LangGraph; "Temporal's OpenAI Agents plugin" for Temporal; "Claude Code's OpenTelemetry spans" for the Claude Agent SDK.
   - Tiers: `captured` for every page with a captured `<Backing>`, `setup only` for Temporal and any page whose case was dropped.
   - Add a LinkCard to the Frameworks CardGrid for each captured row, in table order:
     - Instructor: "Structured outputs and their model calls."
     - Agno: "Agent runs, model calls and tools."
     - smolagents: "Agent steps, model calls and tools."
     - Haystack: "Pipelines and model calls."
     - BeeAI Framework: "Agents in Python."
     - Guardrails AI: "Guards, validators and the checked call."
     - Deep Agents: "LangChain Deep Agents."
     - LangGraph: "Graphs, nodes and tools."
     - Claude Agent SDK: "Claude Code turns inside your app."
   - The LangChain row and card become title "LangChain".

2. **Sidebar** Frameworks group: add the ten pages, alphabetical by label. Each label equals its page's title. Rename "LangChain and LangGraph" to "LangChain".

3. **Regenerate** the skill references.

4. **Design and decisions.**
   - `00-design.md`: add D-IN-22 to the decisions table, with this plan's row as its text.
   - Amend D-IN-7's cell: drop `guardrails` from its list and cite D-IN-22.
   - In § Agent frameworks:
     - the Temporal row's route becomes "`temporalio-openai-agents`' `OpenAIAgentsPlugin(use_otel_instrumentation=True)`; setup only until a pinned dev-server binary exists";
     - the Guardrails row's route becomes "Guardrails' own spans, plus `instrument=["openai"]`";
     - the BeeAI row says Python only.
   - Add "BeeAI Framework (TypeScript)" to § Not supported: "the OpenInference instrumentation supports only 2025 versions (`>=0.1.9 <0.1.14`) and exports no span on current ones".
   - `mvp-decisions.md` § Track IN: add D-IN-22, and amend D-IN-7 the same way.

5. **`CHANGELOG.md` `[Unreleased]` `### Added`:** "Integration pages for Instructor, Agno, smolagents, Haystack, BeeAI Framework, Guardrails AI, Deep Agents, Temporal, LangGraph and the Claude Agent SDK, with captured spans for seven of them; `cetra-sdk` extras `instructor`, `agno`, `smolagents`, `haystack` and `beeai`."

6. **`docs/roadmap.md`, IN row:**
   - Plan cell adds "M3: [`03-m3-frameworks.md`](plans/in-integrations/03-m3-frameworks.md)".
   - Status becomes "**M1–M3 implemented** (…; M3: ten framework pages, seven captured frameworks; Temporal's capture waits for a pinned dev-server binary)".
   - Decisions become `D-IN-1`–`D-IN-22`.

**Done when:**

```bash
cd website && node --test scripts/ src/marketing/ && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/check-integrations.mjs && node scripts/skill-references.mjs
just check-docs
```

All pass. Re-run `just check-docs` once if Astro segfaults (exit 139).

**Commit:** `List M3's framework pages and record D-IN-22`

## After the last phase

The main session runs `just check` once. Before it, touch the crates that embed `CARGO_MANIFEST_DIR`, because the shared build directory can hold another session's binaries. Then a Sonnet code review of M3, its fixes through Haiku, and a push to PR #242.
