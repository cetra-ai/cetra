# W8 — Integrations

**Commit subject:** `Document every integration`

**Goal:** the Integrations topic: a matrix page and one page per framework, provider and instrumentation library the SDKs or the normalizer support (README § Page list › Integrations topic, 18 pages). Each page says what it is backed by: a captured fixture in `fixtures/otlp/` or an SDK test.

## Read first

- README § Page list › Integrations topic (the table is the scope: pages, languages, mechanism, backing); § Style guide.
- `website/STYLE.md`.
- `sdks/python/README.md` § Install (the `instrument` table and extras), § Instrumentation, § Existing OpenTelemetry setups; `sdks/python/src/cetra/` (the instrument registry: `rg -n "openai-agents" sdks/python/src`); `sdks/python/tests/test_instrument.py`.
- `sdks/typescript/README.md` § Instrumentation, § Vercel AI SDK; `sdks/typescript/src/{openai,anthropic,langchain,ai,instrument}.ts`; `instrument.test.ts`, `ai.test.ts`.
- `fixtures/otlp/README.md`; for each page, the `case.json` of every case named in its "Backed by" cell.
- `tools/capture/py/scenarios/<group>/` and `tools/capture/ts/scenarios/` (how each fixture was produced: the packages and the exporter setup are the closest thing to a tested recipe for frameworks the SDK does not wrap).
- `docs/spec/02-ingestion.md` § Conventions.

**Check first:** the Python `instrument` table in `sdks/python/README.md` lists exactly `openai`, `anthropic`, `langchain`, `llama-index`, `litellm`, `google-genai`, `bedrock`, `openai-agents`, `crewai`, `dspy`, and `sdks/typescript/package.json` `exports` has `./openai`, `./anthropic`, `./langchain`. If either differs, the page list differs too: stop and report.

## Files

| File | Change |
|---|---|
| `website/src/content/docs/docs/integrations/index.mdx` (new) | the matrix |
| `website/src/content/docs/docs/integrations/<slug>.mdx` (new, 17 files) | one per row of README § Page list › Integrations topic |
| `website/src/components/Backing.astro` (new) | the "Backed by" box |
| `sdks/typescript/src/examples.test-helpers.ts`, `examples.test.ts` | the TypeScript integration examples |
| `website/astro.config.mjs`, `index.mdx`, `concepts/conventions.mdx`, `get-started/quickstart.mdx` | sidebar, cards, links |

## Steps

### 1. The page template

Every integration page, in this order:

1. Frontmatter: `title:` the framework's name alone ("OpenAI", "LangChain and LangGraph"); `description:` "Trace <framework> calls with cetra: …" in under 160 characters.
2. One sentence: what gets traced.
3. `<Backing />` (step 2) with the page's fixture groups and cases, or `sdk-test` and the test name.
4. **Install**: `<Tabs syncKey="lang">` per supported language: the package and extra (`pip install "cetra-sdk[openai]" openai`), (`npm install @cetra-ai/sdk @arizeai/openinference-instrumentation-openai openai`).
5. **Set up**: the smallest complete program per language, with the provider call.
   - Python: the SDK's test environment does not install the extras, and `init(instrument=[…])` raises without them (README § Instrumentation), so these blocks are `{/* untested: needs cetra-sdk[<extra>] and a <provider> key */}`. The `instrument` name itself is tested by `sdks/python/tests/test_instrument.py`; name that test in `<Backing tests={…}>`.
   - TypeScript: `openai`, `@anthropic-ai/sdk`, `@langchain/core` and the three OpenInference packages are dev dependencies of the SDK. For OpenAI, Anthropic and LangChain, put the `init({ instrument: [openai(OpenAI)] })` setup in its own block marked `{/* example:integration-openai */}` (and `-anthropic`, `-langchain`), add the three functions to `examples.test-helpers.ts` (no provider call: construct the helper and call `init` only) with an `EXAMPLES_BY_NAME` entry asserting what `instrument.test.ts` asserts for that helper. The block that calls the provider is `{/* untested: calls <provider> */}`. The Vercel AI SDK page reuses `{/* example:ai-telemetry */}`.
6. **What you get**: the span kinds and fields this integration fills (model, tokens, cost, messages, tool calls, streaming time to first token), from the fixtures' `expected.json`. Only what the fixtures show.
7. **Limits**: what is not captured, version ranges the fixtures cover (Vercel AI SDK v4–v7), known gaps from the README § Known limits.
8. **Next**: links to the trace view, cost, and the relevant evaluation page once it exists.

Pages for instrumentation libraries and other SDKs (`openinference`, `openllmetry`, `opentelemetry-genai`, `google-adk`, `from-langfuse`, `from-laminar`) replace steps 4–5 with "Point the exporter at cetra": the library's own OTLP exporter configuration (endpoint and `Authorization` header), taken from the capture scenario of that group, marked `{/* untested: <library> setup, captured in fixtures/otlp/<group> */}`; and in the migration pages, a section "What carries over" (sessions, users, metadata, scores do or do not map, from the fixtures) and "What changes".

### 2. `Backing.astro`

```astro
---
// The evidence an integration page rests on (README § Page list › Integrations).
interface Props {
  /** Captured fixture cases, as `group/case`. */
  fixtures?: string[];
  /** SDK tests, as `path::name`. */
  tests?: string[];
}
const { fixtures = [], tests = [] } = Astro.props;
const repo = "https://github.com/cetra-ai/cetra/tree/main";
---
<aside class="backing">
  <strong>Tested against</strong>
  <ul>
    {fixtures.map((f) => <li><a href={`${repo}/fixtures/otlp/${f}`}>captured spans: <code>{f}</code></a></li>)}
    {tests.map((t) => <li>SDK test <code>{t}</code></li>)}
  </ul>
</aside>
<style>
  .backing { border-left: 3px solid var(--sl-color-accent); padding: 0.5rem 1rem; margin: 1rem 0; }
  .backing ul { margin: 0.25rem 0 0; }
</style>
```

A page with neither fixtures nor tests cannot exist: if a framework has neither, leave its page out and say so in the report.

`website/scripts/check-backing.mjs` (with `check-backing.test.mjs`, in `check:content`): every `fixtures={[…]}` value on a page names an existing directory `fixtures/otlp/<group>/<case>`.

### 3. The matrix

**`integrations/index.mdx`** (title "Integrations"): one sentence; a table `| Integration | Python | TypeScript | How | Tested against |` with one row per page (✓ or —, the mechanism in a few words, "captured" or "SDK test"); then groups as `<CardGrid>`: Providers (OpenAI, Anthropic, Google Gen AI, Amazon Bedrock, LiteLLM), Frameworks (LangChain and LangGraph, LlamaIndex, Vercel AI SDK, OpenAI Agents SDK, CrewAI, DSPy, Google ADK), Instrumentation libraries (OpenInference, OpenLLMetry, OpenTelemetry GenAI), Migrating (from Langfuse, from Laminar). A last paragraph: anything else that speaks OpenTelemetry works through `/docs/get-started/opentelemetry/`.

### 4. Sidebar and links

Add the topic after Docs:

```js
{
  label: "Integrations",
  link: "/docs/integrations/",
  icon: "puzzle",
  items: [
    { label: "All integrations", link: "/docs/integrations/" },
    {
      label: "Providers",
      items: [
        { label: "OpenAI", link: "/docs/integrations/openai/" },
        { label: "Anthropic", link: "/docs/integrations/anthropic/" },
        { label: "Google Gen AI", link: "/docs/integrations/google-genai/" },
        { label: "Amazon Bedrock", link: "/docs/integrations/bedrock/" },
        { label: "LiteLLM", link: "/docs/integrations/litellm/" },
      ],
    },
    {
      label: "Frameworks",
      items: [
        { label: "LangChain and LangGraph", link: "/docs/integrations/langchain/" },
        { label: "LlamaIndex", link: "/docs/integrations/llama-index/" },
        { label: "Vercel AI SDK", link: "/docs/integrations/vercel-ai-sdk/" },
        { label: "OpenAI Agents SDK", link: "/docs/integrations/openai-agents/" },
        { label: "CrewAI", link: "/docs/integrations/crewai/" },
        { label: "DSPy", link: "/docs/integrations/dspy/" },
        { label: "Google ADK", link: "/docs/integrations/google-adk/" },
      ],
    },
    {
      label: "Instrumentation libraries",
      items: [
        { label: "OpenInference", link: "/docs/integrations/openinference/" },
        { label: "OpenLLMetry", link: "/docs/integrations/openllmetry/" },
        { label: "OpenTelemetry GenAI", link: "/docs/integrations/opentelemetry-genai/" },
      ],
    },
    {
      label: "Migrating",
      items: [
        { label: "From Langfuse", link: "/docs/integrations/from-langfuse/" },
        { label: "From Laminar", link: "/docs/integrations/from-laminar/" },
      ],
    },
  ],
},
```

`icon` names must exist in Starlight's icon set; if `puzzle` does not, use `seti:plug` or another listed in `@astrojs/starlight/components/Icons.ts` and say so. `index.mdx`: an "Integrations" card. `concepts/conventions.mdx`: each convention row links its page. Quick start § Next steps: "Using a framework? See Integrations."

## Tests

- `check-backing.test.mjs` passes; a page naming a missing fixture fails `check-backing.mjs`.
- `just check-ts` runs the new `integration-*` examples.
- `just check-docs` passes.

## Done when

- `just check` passes; 18 pages exist; every page has a `<Backing>` box; no framework is documented without one.
