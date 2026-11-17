# W · Documentation site

Status: design and plan written on 2026-11-17; nothing implemented.

Track W builds cetra's user documentation: a full, production-ready site comparable to Laminar's and Langfuse's, served at **`https://cetra.sh/docs/`**. It is built with Starlight (Astro) from a new top-level directory, **`website/`**, in this repository, so a change to the code and its documentation land in the same PR. The same Astro project serves the landing page at `https://cetra.sh/` (today's one-page holding site, ported as-is) and will later hold the marketing site.

This README is the design. The plan is thirteen sub-phases, **W1–W13**, one file each and one commit each, in order. W1 and W2 ship a live, deployed site with a real overview page; W3 makes it useful (a quick start that works); every later phase adds content or a drift check to a site that is already live.

## Contents

- [Read first, for every sub-phase](#read-first-for-every-sub-phase)
- [Files](#files)
- [Decisions](#decisions)
- [Information architecture](#information-architecture)
- [Page list](#page-list)
- [Source of truth and drift](#source-of-truth-and-drift)
- [Tooling and versions](#tooling-and-versions)
- [Deployment](#deployment)
- [Versioning policy](#versioning-policy)
- [Analytics, SEO and agents](#analytics-seo-and-agents)
- [Screenshots](#screenshots)
- [Style guide](#style-guide)
- [What the user does by hand](#what-the-user-does-by-hand)
- [Open questions](#open-questions)

## Read first, for every sub-phase

- `AGENTS.md`;
- this README, all of it;
- the sub-phase file;
- `website/STYLE.md` once W1 has written it;
- every existing file the sub-phase names. Read a file before you edit it.

Nothing in a phase file may need a guess. If the code or a tool differs from what a phase file quotes (a config option was renamed, a command prints something else), stop and report instead of improvising. The versions below were looked up on 2026-11-17; use exactly them.

## Files

| File | Sub-phase |
|---|---|
| [01-scaffold.md](01-scaffold.md) | W1 `website/`: Astro + Starlight in the pnpm workspace, the landing page, the overview page, the style guide, `just check-docs`, the CI job |
| [02-deploy.md](02-deploy.md) | W2 Cloudflare Pages: `docs-deploy.yml`, PR previews, production on `main`, headers, robots, sitemap |
| [03-get-started.md](03-get-started.md) | W3 the quick start (Python, TypeScript, OpenTelemetry), "Run cetra", the plain-OpenTelemetry page; tested samples in docs pages |
| [04-self-hosting.md](04-self-hosting.md) | W4 `docs/self-hosting/` moves into the site; the configuration reference and its two-way test against the loader; the env-var check |
| [05-api-reference.md](05-api-reference.md) | W5 the API reference generated from `crates/server/openapi.json` at build time; the API overview and OTLP reference pages |
| [06-cli-reference.md](06-cli-reference.md) | W6 the CLI reference generated from clap, with an up-to-date test |
| [07-concepts-tracing.md](07-concepts-tracing.md) | W7 concepts and the tracing feature pages; the attribute, filter-language and SQL-schema references |
| [08-integrations.md](08-integrations.md) | W8 one page per framework and instrumentation, the integrations matrix |
| [09-evaluation.md](09-evaluation.md) | W9 scores, annotation, evaluators and LLM judges, LLM connections, online rules, datasets, experiments |
| [10-platform.md](10-platform.md) | W10 alerts, MCP, keys, orgs and roles, SSO, deletion and retention, plans and licence; the self-hosting auth and email pages |
| [11-sdk-reference.md](11-sdk-reference.md) | W11 the Python and TypeScript SDK references, moved out of the READMEs, with symbol-coverage tests |
| [12-screenshots.md](12-screenshots.md) | W12 reproducible screenshots: Playwright against the web UI on its MSW mocks |
| [13-help-closeout.md](13-help-closeout.md) | W13 FAQ, troubleshooting, changelog, support, `llms.txt`, the weekly external link check; closeout |

### Files W touches (for sequencing with other sessions)

W owns `website/` and `docs/plans/docs-site/`. Outside them it edits only:

| File | Sub-phases |
|---|---|
| `pnpm-workspace.yaml`, `pnpm-lock.yaml`, `biome.json`, `.gitignore` | W1 (W12 adds `playwright` to the lockfile) |
| `justfile` | W1, W2, W6, W7, W12, W13 |
| `.github/workflows/ci.yml` | W1 (the `docs` job and `ci-ok`) |
| `.github/workflows/docs-deploy.yml` (new) | W2 |
| `.github/workflows/nightly.yml` | W13 |
| `AGENTS.md` | W1, W4, W13 |
| `docs/self-hosting/` (moved and deleted) | W4 |
| `crates/server/src/config.rs` (a test) | W4 |
| `crates/cli/src/main.rs` (a URL in one message and its test; a test) | W4, W6 |
| `deploy/compose/*.yaml`, `deploy/compose/cetra.env.example`, `deploy/helm/cetra/README.md`, `values.yaml`, `templates/NOTES.txt` (links) | W4 |
| `crates/storage/tests/s3.rs`, `crates/storage/src/model_tests/mod.rs` (comments that cite `docs/self-hosting/`) | W4 |
| `crates/server/src/api/openapi.rs` (the API description) | W5 |
| `crates/server/src/api/schema.rs`, `crates/core/src/names.rs` (tests) | W7 |
| `sdks/python/tests/test_readme.py` → `test_examples.py`, `sdks/python/README.md` | W3, W11 |
| `sdks/typescript/src/readme.test.ts` → `examples.test.ts`, `readme-examples.test-helpers.ts` → `examples.test-helpers.ts`, `sdks/typescript/README.md` | W3, W11 |
| `docs/spec/12-operations.md`, `docs/spec/README.md`, `docs/spec/10-sdk.md` (pointers) | W4, W11 |
| `CHANGELOG.md` | W4, W11 |
| `docs/roadmap.md`, `docs/mvp-decisions.md` | W13 |

## Decisions

The decisions are also recorded in `docs/mvp-decisions.md` § Track W.

| # | Topic | Decision |
|---|---|---|
| D-W-1 | Directory | **`website/`**, a pnpm workspace package `@cetra/website`. Not `docs-site/`: the same Astro project will hold the marketing site (the coordinator's reason for Astro), so the directory is named for the site, not for one section. Not `docs/`: that directory holds the internal specs, plans and decisions, and stays so. |
| D-W-2 | URL | **`https://cetra.sh/docs/`**, with the landing page at `https://cetra.sh/` from the same Astro project and the same Cloudflare Pages project, `cetra-site`. A path, not `docs.cetra.sh`: search engines credit one host with the links the docs earn (the docs will draw most of the traffic), the marketing site and the docs share navigation, analytics and one deploy, and Langfuse and Laminar both do the same. The cost is that the apex moves to the new Pages project in W2; the landing page is the holding page ported unchanged, so `https://cetra.sh/` looks the same. Starlight content lives under `website/src/content/docs/docs/` so its routes start with `/docs/`; `src/pages/index.astro` is the landing page. `trailingSlash: "always"`. |
| D-W-3 | Deployment | **Direct upload from GitHub Actions with `wrangler pages deploy`, not Cloudflare's Git integration.** The site is built by the same `just` recipe, Node, pnpm and lockfile as CI (`.node-version` is 26, pnpm is 11 with `minimumReleaseAge`; Cloudflare's build image would be a second toolchain). `wrangler` is an exact-pinned devDependency of `website/`. A new workflow, `docs-deploy.yml`, deploys `main` to production and each same-repository PR to a preview branch `pr-<number>`, owner-gated like the S3 cache (`ci.yml`'s `CI_CACHE_ALLOWED`). It is outside `ci-ok`: a Cloudflare outage must not block merges. The build check is in `ci.yml` (`docs` job, in `ci-ok`). |
| D-W-4 | Information architecture | **One `/docs/` root with four sidebar topics: Docs, Integrations, Reference, Self-hosting** (`starlight-sidebar-topics`). Langfuse's six roots (docs, integrations, self-hosting, guides, academy, faq) make it easy to get lost; Laminar's single sidebar mixes basics with advanced features and lists 35 integrations flat. Docs follows Laminar's per-area shape (overview, then features, then troubleshooting links); Integrations is grouped by kind (providers, frameworks, instrumentation libraries, migration) like Langfuse; the quick start is one page with synced language tabs (Python, TypeScript, OpenTelemetry), like Langfuse's, without its agent-install preamble. Full list in § Page list. |
| D-W-5 | Source of truth | **User documentation lives in `website/` only.** `docs/self-hosting/` moves into the site and is deleted (W4). The SDK READMEs keep install, the quick start and a link to the site, because they are the PyPI and npm pages (W11). The specs stay the internal contract; a doc page never cites a spec or a decision ID. A user-visible change updates its doc page in the same PR (`AGENTS.md` rule, W1). |
| D-W-6 | Tested samples | **Every `python`, `ts`, `tsx` or `js` code block in the site is tested or carries `{/* untested: <reason> */}`; `website/scripts/check-samples.mjs` fails otherwise.** Python blocks marked `{/* test */}` run in `sdks/python/tests/test_examples.py` (the README test, generalised); `{/* test:otel */}` blocks run against a recording OTLP endpoint. TypeScript blocks marked `{/* example:<name> */}` must be byte-identical to the body of a function in `sdks/typescript/src/examples.test-helpers.ts`, which `examples.test.ts` runs (the README mechanism, generalised). `bash`, `toml`, `yaml`, `json` and `sh` blocks are not code samples. |
| D-W-7 | Generated references | **The API reference is rendered at build time from `crates/server/openapi.json`** by `starlight-openapi`, after `website/scripts/public-openapi.mjs` drops the `internal` tag (the control plane's routes). **The CLI reference and the SQL schema reference are generated** from clap and from the server's schema catalog into committed pages, each with a test that fails when the page is stale (`CETRA_DOCS_WRITE=1`, through `just docs-gen`, rewrites them, as `CETRA_OPENAPI_WRITE=1` does for `openapi.json`). **The span attribute reference is written by hand and checked against `cetra-core`'s attribute names.** **The configuration reference is written by hand and tested both ways against the loader** (W4). The changelog page is generated from `CHANGELOG.md` at build time. |
| D-W-8 | SDK reference | **Hand-written, with symbol-coverage tests**, not typedoc or a Python doc generator: the API surface is small (Python `cetra.__all__`, the TypeScript package exports), the README prose is already reference-quality, and generated pages would read worse than Laminar's hand-written ones. A Python test and a TypeScript test check that every exported name has a heading in the reference page and every heading names an export. |
| D-W-9 | Versions | **One version of the docs, built from `main`.** Before the first release a site-wide banner says cetra is not released yet. After it, a feature merged since the latest release shows an "Unreleased" badge (`<Since v="x.y.z" />`, compared at build time with the latest released version in `CHANGELOG.md`). Versioned snapshots (`starlight-versions`) only from the first breaking major after 1.0, recorded as a new decision then. |
| D-W-10 | Analytics | **Cloudflare Web Analytics, turned on in the Pages project settings, nothing in the repository.** It sets no cookie and collects no personal data, so there is no consent banner. No other analytics, no third-party search or chat widget. |
| D-W-11 | Search | **Pagefind** (built into Starlight): static, no service, no data leaves the page. No AI search in the MVP; `llms.txt` (W13, `starlight-llms-txt`) serves coding agents. |
| D-W-12 | Screenshots | **A Playwright script against the web UI's dev server on its MSW mocks (`fixtures/api`)**, with a fixed clock, light theme and a 1440×900 viewport at device scale 2, writes PNGs committed under `website/src/assets/screenshots/`. Run by hand with `just docs-screenshots` when the UI changes, not in CI (pixel output differs across machines; a CI diff would be flaky). CI checks that every listed screenshot exists. |
| D-W-13 | Link checking | **Internal links fail the build** (`starlight-links-validator`, which also checks anchors). **External links are checked weekly** by `lychee` in `nightly.yml`, outside `ci-ok`, opening or updating the `nightly` issue like the fuzz job. |
| D-W-14 | Claims | **No performance numbers.** The 5M-trace benchmark missed some query targets; the docs describe how cetra works and its limits and resource needs, never "N spans/s" or "X× faster". A number can come in only with a published benchmark page that gives its method and hardware, through a new decision. |
| D-W-15 | Style | **`website/STYLE.md`** (§ Style guide) is the voice and terminology guide; `website/scripts/check-terms.mjs` enforces its banned terms in CI. |

## Information architecture

### What the two references do

Laminar (`laminar.sh/docs`, Mintlify): one sidebar; an overview that is a card grid; one "Getting started" page plus a quickstart per product area; each area shaped introduction → quickstart → concepts → troubleshooting; 35 integrations in one flat list titled "X observability"; hand-written SDK reference; an OpenAPI-generated API reference; four self-hosting pages; a changelog; `llms.txt` and every page as Markdown. Weak points: tracing basics and advanced features share one section, "Platform" is a catch-all, the flat integration list is hard to scan.

Langfuse (`langfuse.com/docs`, Nextra): several roots (`/docs`, `/integrations`, `/self-hosting`, `/guides`, `/faq`, `/changelog`); a get-started page with steps and framework tabs (OpenAI, Vercel AI SDK, LangChain, the SDKs, OpenTelemetry), followed by "not seeing what you expected?" links; a Features folder of about 30 small pages; integrations grouped by kind (native OTel, frameworks, model providers, gateways); about 50 self-hosting pages; the API reference on its own host. Weak points: too many roots, a 29k-character get-started page that repeats itself across tabs, overlapping overview pages.

### What cetra takes

- From Langfuse: one quick start with tabs per language, a "nothing showing up?" section at its end, integrations grouped by kind, small single-feature pages, self-hosting as a section of its own.
- From Laminar: one root, the per-area shape, consistent integration titles (the framework's name alone), the API reference generated next to the docs, `llms.txt`.
- Neither: no academy, no guides root (recipes go in the feature page they belong to), no AI chat.

The four sidebar topics, each with its own sidebar:

| Topic | Landing page | Holds |
|---|---|---|
| Docs | `/docs/` | Overview, Get started, Concepts, Tracing, Evaluation, Platform, Help |
| Integrations | `/docs/integrations/` | the matrix, providers, frameworks, instrumentation libraries, migration |
| Reference | `/docs/reference/` | SDKs, API, OTLP, CLI, attributes, filter language, SQL schema |
| Self-hosting | `/docs/self-hosting/` | install, configuration, operations |

The header shows the four topics as tabs; Pagefind searches all of them.

## Page list

URLs are relative to `https://cetra.sh/docs/`. The files are `website/src/content/docs/docs/<path>.mdx` (`index.mdx` for a directory). "Sources" are where the content comes from; the page itself never links to a spec. Every page lists the phase that writes it.

### Docs topic

**Start**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `` (index) | Overview | What cetra is (traces, cost, evaluation, alerts for LLM apps; one binary; OpenTelemetry-native), the loop instrument → inspect → evaluate → alert, cards to the four topics and to the quick start | holding page, `AGENTS.md` § Project, spec README | W1 |
| `get-started/quickstart` | Quick start | Run cetra locally (Docker one-liner), create a project and its `sk-cetra-` key, send a first trace with synced tabs Python / TypeScript / OpenTelemetry, open it in the UI; "Nothing showing up?" checklist | SDK READMEs § Quick start, `self-hosting/install.md`, spec 09 § OTLP | W3 |
| `get-started/run-cetra` | Run cetra | The ways to run it (Docker, binary, compose, Helm) with one paragraph and a link each; first start (the setup page, or `CETRA_INIT_*` for headless); ports | `self-hosting/README.md`, `install.md`, `configuration.md` | W3 |
| `get-started/opentelemetry` | Send OpenTelemetry | cetra without our SDKs: OTLP/HTTP and gRPC endpoints and ports, the `Authorization: Bearer sk-cetra-…` header, Python and TypeScript exporter setup (tested), Go and Java (untested), an OpenTelemetry Collector pipeline, which conventions are understood (link to Concepts) | spec 02 § Ingestion, spec 09 § OTLP | W3 |

**Concepts**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `concepts/data-model` | Data model | Orgs, projects, environments; traces, spans and span kinds; sessions; users; scores; datasets and experiments; IDs and timestamps as a user sees them | spec 01 | W7 |
| `concepts/how-cetra-works` | How cetra works | One process: ingestion, normalization, the storage engine (WAL, memtable, Parquet), the query layer, the UI and API; what is durable when (acknowledged spans survive a crash); no numbers (D-W-14) | specs 02, 03, 05 | W7 |
| `concepts/conventions` | Semantic conventions | Which instrumentation conventions are read (OpenTelemetry GenAI, OpenInference, OpenLLMetry, Vercel AI SDK, Langfuse and Laminar SDK spans, Google ADK) and how each maps to cetra's fields; precedence when several are present | spec 02 § Conventions, `fixtures/otlp/` | W7 |
| `concepts/glossary` | Glossary | One line per term of § Style guide › Terminology | `website/STYLE.md` | W7 |

**Tracing**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `tracing/traces` | Traces | The trace list (columns, layouts), the trace view (tree, timeline, span detail, input and output rendering, chat messages, tool calls), live updates | spec 11, spec 09 § traces/spans | W7 |
| `tracing/sessions` | Sessions | Grouping traces into a session (SDK context, attributes), the session list and view | spec 01, spec 11, SDK READMEs § Context | W7 |
| `tracing/users` | Users | User IDs on traces, the users list, per-user cost and activity, deleting a user's data (link) | spec 01, spec 11 | W7 |
| `tracing/metadata-and-tags` | Metadata, tags, environments and releases | What each is for, how to set it from the SDKs and from plain attributes, how to filter on it | spec 01, SDK READMEs | W7 |
| `tracing/filters-and-search` | Filters and search | The filter bar, the filter language by example, full-text search over inputs and outputs, saved views | spec 05 § Filter DSL, spec 11 | W7 |
| `tracing/sql-console` | SQL console | Querying your data with SQL, the tables (link to the schema reference), limits and timeouts, saved queries, CSV/JSON export, the CLI `cetra query` | spec 05 § SQL, spec 09 § sql | W7 |
| `tracing/dashboard` | Dashboard | The project dashboard: what each chart measures, the time range | spec 11, spec 05 § metrics | W7 |
| `tracing/cost-and-tokens` | Cost and tokens | Token usage fields read from each convention, how cost is computed, the built-in price table, custom model prices per org and project, reported versus computed cost | spec 02 § Cost, track T's spec sections | W7 |
| `tracing/media` | Media | Images, audio and files in inputs and outputs: extraction, size bounds, storage, how the UI shows them | spec 02 § Media | W7 |
| `tracing/privacy-controls` | Masking, sampling and truncation | SDK side: `capture_content`, masking, sampling, export filters; server side: truncation limits and what a truncated value looks like | SDK READMEs § Masking, spec 02 § Truncation | W7 |

**Evaluation**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `evaluation/` | Evaluation overview | How scores, evaluators, online rules, datasets and experiments fit together; which to use when | spec 06 | W9 |
| `evaluation/scores` | Scores | Score types and values, writing scores from the SDKs and the API, the public feedback endpoint and browser feedback, score definitions | spec 06, spec 09 § scores, SDK READMEs § Scores | W9 |
| `evaluation/annotation` | Human annotation | The annotation form on a trace, score definitions as the form's fields, who owns a score | spec 06, spec 11 | W9 |
| `evaluation/evaluators` | Evaluators and LLM judges | Evaluator kinds (checks, LLM judges, webhooks), variable mapping, templates, test runs, the judge's own trace | spec 06 | W9 |
| `evaluation/llm-connections` | LLM connections | Adding provider credentials for judges, how they are encrypted, egress rules (private networks) | spec 06, spec 08 | W9 |
| `evaluation/online-rules` | Online rules | Running evaluators on live traces: filters, sampling, triggers (trace completion), cost caps, previews, backfills, running an evaluator on one trace | spec 06 | W9 |
| `evaluation/datasets` | Datasets | Versioned datasets, items, CSV and JSONL import, export, adding items from a trace | spec 06, spec 09 § datasets | W9 |
| `evaluation/experiments` | Experiments | `evaluate()` in Python and TypeScript (tested), what gets recorded, the results and compare views | SDK READMEs § Experiments, spec 10 | W9 |

**Platform**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `platform/alerts` | Alerts | Alert rules over metrics, conditions and windows, channels (Slack, webhook, email), delivery and retries, the Free plan's limit where it applies | spec 07 | W10 |
| `platform/mcp` | MCP server | Connecting a coding agent (Claude Code, Cursor and others) over streamable HTTP with a personal access token; the seven read-only tools; limits | spec 09 § MCP | W10 |
| `platform/api-keys` | API keys and tokens | `sk-cetra-` (ingest and API), `pk-cetra-` (public, browser feedback), `pat-cetra-` (personal, CLI and MCP); scopes; rotation | spec 08, spec 09 | W10 |
| `platform/organizations` | Organizations, projects and roles | Orgs, projects, members, invites, roles and what each may do (RBAC) | spec 08 § Roles | W10 |
| `platform/sso` | Single sign-on | Sign-in methods a user sees: password, GitHub, Google, OIDC; per-org SSO with verified domains (Teams add-on); links to the self-hosting setup page | spec 08 | W10 |
| `platform/deletion-and-retention` | Deletion and retention | Deleting by user, trace, filter, project or org; what is deleted and when; retention per project; a GDPR erasure walk-through | spec 03 § Deletion, track M's spec sections | W10 |
| `platform/plans-and-licence` | Plans and licence | Open core: what is MIT, what is under the Elastic License and locked without a key; licence keys on a self-hosted instance; the usage page | spec 12 § Licence keys | W10 |

**Help**

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `help/faq` | FAQ | Short answers: OpenTelemetry compatibility, data ownership and the Parquet format, cloud versus self-hosted, licence, migration from Langfuse or Laminar, supported languages | everything above | W13 |
| `help/troubleshooting` | Troubleshooting | By symptom: no traces (key, endpoint, port, flush on exit, serverless), 401/403/413/429 on ingest, missing content, missing cost, wrong nesting, clock skew, gRPC through a proxy, the UI says "not embedded" | SDK READMEs § Known limits, spec 09 problem codes, spec 12 | W13 |
| `help/changelog` | Changelog | Generated from `CHANGELOG.md` at build time | `CHANGELOG.md` | W13 |
| `help/support` | Support and security | GitHub issues and discussions, contact address, how to report a vulnerability | holding page | W13 |

### Integrations topic

The matrix page and one page per integration, titled with the framework's name. Every page: install, the smallest working setup (tested where the SDK runs it), what gets captured, known gaps, the captured fixtures it is backed by (`fixtures/otlp/<group>/`). A framework with no captured fixture says what was tested instead.

| Path | Title | Languages | How | Backed by | Phase |
|---|---|---|---|---|---|
| `integrations/` | Integrations | — | matrix: framework × language × mechanism × captured | — | W8 |
| `integrations/openai` | OpenAI | Python, TypeScript | `instrument=["openai"]` / `openai(OpenAI)` | `openinference/openai-*`, `openllmetry/openai-*`, `genai/openai-v2-*` | W8 |
| `integrations/anthropic` | Anthropic | Python, TypeScript | `anthropic` | `openinference/anthropic-*`, `openllmetry/anthropic-*` | W8 |
| `integrations/google-genai` | Google Gen AI | Python | `google-genai` | `openinference/google-genai-*`, `genai/google-genai-*` | W8 |
| `integrations/bedrock` | Amazon Bedrock | Python | `bedrock` | SDK instrument test | W8 |
| `integrations/litellm` | LiteLLM | Python | `litellm` | SDK instrument test | W8 |
| `integrations/langchain` | LangChain and LangGraph | Python, TypeScript | `langchain` | `openinference/langchain-*`, `openinference/langgraph-agent`, `openllmetry/lang*` | W8 |
| `integrations/llama-index` | LlamaIndex | Python | `llama-index` | SDK instrument test | W8 |
| `integrations/vercel-ai-sdk` | Vercel AI SDK | TypeScript | `aiTelemetry()` | `vercel/*` (v4–v7) | W8 |
| `integrations/openai-agents` | OpenAI Agents SDK | Python | `openai-agents` | `openinference/openai-agents-tool`, `openllmetry/openai-agents-tool` | W8 |
| `integrations/crewai` | CrewAI | Python | `crewai` | SDK instrument test | W8 |
| `integrations/dspy` | DSPy | Python | `dspy` | SDK instrument test | W8 |
| `integrations/google-adk` | Google ADK | Python | ADK's own OpenTelemetry export | `google_adk/*`, `openinference/google-adk-agent-tool` | W8 |
| `integrations/openinference` | OpenInference | any | point its exporter at cetra | `openinference/*` | W8 |
| `integrations/openllmetry` | OpenLLMetry (Traceloop) | any | point its exporter at cetra | `openllmetry/*` | W8 |
| `integrations/opentelemetry-genai` | OpenTelemetry GenAI instrumentations | any | the official `opentelemetry-instrumentation-*` packages | `genai/*` | W8 |
| `integrations/from-langfuse` | Migrating from Langfuse | Python, TypeScript | keep the Langfuse SDK, export its spans to cetra | `langfuse/*` | W8 |
| `integrations/from-laminar` | Migrating from Laminar | Python, TypeScript | keep the Laminar SDK, export its spans to cetra | `laminar/*` | W8 |

### Reference topic

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `reference/` | Reference | Index of the pages below | — | W5 |
| `reference/python/` | Python SDK | Install, extras, configuration (options, env vars, validation) | `sdks/python/README.md` | W11 |
| `reference/python/tracing` | Tracing (Python) | `observe` and its rules, manual spans, `llm_span`, generators, errors | README § Decorator rules, § Manual spans | W11 |
| `reference/python/context` | Context (Python) | Sessions, users, metadata and tags through context and baggage; threads and async | README § Context, § Threads | W11 |
| `reference/python/scores-datasets-experiments` | Scores, datasets and experiments (Python) | `score`, the `Client`, datasets, `evaluate`, what gets recorded, keys, errors | README § Scores, § Datasets and experiments | W11 |
| `reference/python/opentelemetry` | OpenTelemetry (Python) | `instrument`, existing OTel setups, masking and `capture_content`, serverless, known limits, what is never logged | README § Instrumentation onwards | W11 |
| `reference/python/api` | API (Python) | Every public name of `cetra.__all__`: signature, one paragraph, link to the guide section; tested for coverage | `sdks/python/src/cetra/` | W11 |
| `reference/typescript/` | TypeScript SDK | Install, peers, configuration | `sdks/typescript/README.md` | W11 |
| `reference/typescript/tracing` | Tracing (TypeScript) | `observe`, manual spans, `withLlmSpan`, errors | README § Wrapper rules, § Manual spans | W11 |
| `reference/typescript/context` | Context (TypeScript) | Context and baggage | README § Context | W11 |
| `reference/typescript/scores-datasets-experiments` | Scores, datasets and experiments (TypeScript) | `score`, `Client`, datasets, `evaluate` | README § Scores, § Datasets and experiments | W11 |
| `reference/typescript/opentelemetry` | OpenTelemetry (TypeScript) | Instrumentation helpers, Vercel AI SDK, existing setups, masking | README § Instrumentation onwards | W11 |
| `reference/typescript/runtimes` | Runtimes (TypeScript) | Edge runtimes, serverless, browser feedback | README § Edge runtimes, § Browser feedback | W11 |
| `reference/typescript/api` | API (TypeScript) | Every export of `@cetra-ai/sdk` and its subpaths; tested for coverage | `sdks/typescript/src/index.ts`, `package.json` `exports` | W11 |
| `reference/api/` | REST API | Base URL, authentication (keys, PATs, cookies), errors (problem details and codes), pagination, rate limits, stability before 1.0 | spec 09 | W5 |
| `reference/api/…` | (generated) | One page per operation, grouped by tag, from the public OpenAPI document | `crates/server/openapi.json` | W5 |
| `reference/otlp` | OTLP endpoint | Endpoints and ports, authentication, encodings (protobuf, JSON, gzip), limits and their status codes, the 402 when a plan's cap is reached, what is accepted and dropped (`/v1/logs`, `/v1/metrics`) | spec 02, spec 09 | W5 |
| `reference/cli` | CLI | Generated: every `cetra` command and option (`--help` text) | `crates/cli` | W6 |
| `reference/attributes` | Span attributes | Every `cetra.*` attribute and every convention attribute cetra reads, with type and meaning | spec 01, spec 02 | W7 |
| `reference/filter-language` | Filter language | Grammar, fields, operators, values, examples | spec 05 § Filter DSL | W7 |
| `reference/sql-schema` | SQL schema | Every view of the SQL console and the filter language, with its columns, types and descriptions, and the functions; generated | `GET /schema`'s catalog (`crates/server/src/api/convert.rs`) | W7 |

### Self-hosting topic

| Path | Title | Scope | Sources | Phase |
|---|---|---|---|---|
| `self-hosting/` | Self-hosting | Methods, ports, the one-process-per-directory rule, licences | `docs/self-hosting/README.md` | W4 |
| `self-hosting/install` | Install | Binary (`install.sh`), systemd, Docker | `install.md` | W4 |
| `self-hosting/docker-compose` | Docker Compose | The two compose files (local, Garage S3) | `docker-compose.md` | W4 |
| `self-hosting/kubernetes` | Kubernetes | The Helm chart, values reference | `kubernetes.md`, `deploy/helm/cetra/README.md` | W4 |
| `self-hosting/configuration` | Configuration | Sources and precedence, the common settings, secrets, TLS | `configuration.md` | W4 |
| `self-hosting/configuration-reference` | Configuration reference | Every key with its env form, type and default; tested both ways (§ Source of truth) | spec 12 § Configuration, `config.rs`, `StorageConfig` | W4 |
| `self-hosting/s3` | Object storage (S3) | Parquet files and media in a bucket; providers | `s3.md` | W4 |
| `self-hosting/backup` | Backup and restore | Snapshots, online backups, restore, restore mode | `backup.md` | W4 |
| `self-hosting/monitoring` | Monitoring | Health checks, Prometheus metrics, alert examples | `monitoring.md` | W4 |
| `self-hosting/upgrade` | Upgrade | Upgrading, upgrade notes, migrations | `upgrade.md` | W4 |
| `self-hosting/resources` | Resources | Memory, disk and file-descriptor needs, the small-machine profile; no throughput figures (D-W-14) | spec 03 § 10 | W4 |
| `self-hosting/security` | Security | The security checklist: TLS, trusted proxies, egress to private networks, secrets, the metrics listener | spec 12 § Security checklist | W4 |
| `self-hosting/authentication` | Authentication setup | Sign-up policy, password login, GitHub, Google, OIDC (Okta, Entra ID, Keycloak) with each provider's console steps, instance admins | spec 08, `configuration.md` § Secrets | W10 |
| `self-hosting/email` | Email (SMTP) | `smtp_url`, `email_from`, providers, what emails cetra sends | spec 12, spec 08 | W10 |

## Source of truth and drift

### What moves, what stays

| Today | After W |
|---|---|
| `docs/self-hosting/*.md` | moved to `website/…/self-hosting/` in W4 and deleted; links in code, deploy files and specs point at `https://cetra.sh/docs/self-hosting/<page>/` |
| `sdks/python/README.md` (650 lines) | keeps its title, one-paragraph intro, § Install, § Quick start (tested) and a "Documentation" link; everything else moves to `reference/python/*` in W11 |
| `sdks/typescript/README.md` (423 lines) | the same, to `reference/typescript/*` |
| `deploy/helm/cetra/README.md` | keeps its values table (`helm-check` and the chart's users read it there); `self-hosting/kubernetes` links to it on GitHub and repeats only the common values |
| `docs/spec/*` | unchanged: the internal contract. Spec 12 § Documentation deliverables points at the site |
| `CHANGELOG.md` | unchanged; rendered into `help/changelog` at build time |

### The checks

| Drift | Check | Where it runs | Phase |
|---|---|---|---|
| A Python sample no longer runs | `sdks/python/tests/test_examples.py` runs every `{/* test */}` and `{/* test:otel */}` block of the site and the README | `just check-py` | W3 |
| A TypeScript sample no longer compiles or runs | `sdks/typescript/src/examples.test.ts`: every `{/* example:<name> */}` block equals a typechecked, executed function | `just check-ts` | W3 |
| A sample is neither tested nor marked untested | `website/scripts/check-samples.mjs` | `just check-docs` | W3 |
| An API change | the reference is generated from `openapi.json` at each build; `openapi.json` itself is checked by the server's contract test | `just check-docs` | W5 |
| A CLI change | `cetra-cli` test compares the generated page with the committed one | `just test` | W6 |
| A config key added, renamed or removed | `config::tests::the_configuration_reference_matches_the_loader`: every documented key is accepted, every accepted key is documented, every env alias matches `ALIASES` | `just test` | W4 |
| An env var in prose that no longer exists | `website/scripts/check-env-vars.mjs`: every `CETRA_*` name in the site appears in the code, or (for the `__` split form) names a key of the configuration reference | `just check-docs` | W4 |
| An SDK export added or removed | Python `test_api_reference.py`, TypeScript `api-reference.test.ts` | `just check-py`, `just check-ts` | W11 |
| A SQL view or column changes | `cetra-server` test regenerates the schema page and compares it | `just test` | W7 |
| A `cetra.*` attribute added or removed | `cetra-core` test compares the attribute page with the name constants | `just test` | W7 |
| An integration page cites a fixture that does not exist | `website/scripts/check-backing.mjs` | `just check-docs` | W8 |
| A broken internal link or anchor | `starlight-links-validator` | `just check-docs` (build) | W1 |
| A dead external link | `lychee`, weekly | `nightly.yml` | W13 |
| A screenshot file missing | `website/scripts/check-screenshots.mjs` | `just check-docs` | W12 |
| A banned term or the old name | `website/scripts/check-terms.mjs` | `just check-docs` | W1 |

The `website/scripts/*.mjs` checks are plain Node (no dependency) with unit tests run by `node --test`.

### Rule for every PR

`AGENTS.md` gains (W1): "A user-visible change updates its page under `website/src/content/docs/docs/` in the same PR, as it adds a `CHANGELOG.md` line."

## Tooling and versions

Versions looked up on 2026-11-17 from the npm registry and crates.io, each at least 7 days old (`minimumReleaseAge: 10080`). Exact versions, in `website/package.json`.

| Package | Version | Published | Why |
|---|---|---|---|
| `astro` | 7.4.8 | 2026-11-09 | the framework |
| `@astrojs/starlight` | 0.42.10 | 2026-11-06 | the docs theme; brings Pagefind 1.x, Expressive Code, sitemap and MDX |
| `starlight-openapi` | 0.27.0 | 2026-10-07 | the API reference (peer `@astrojs/starlight >=0.41.0`) |
| `starlight-links-validator` | 0.27.0 | 2026-10-09 | internal link and anchor check (peer `@astrojs/starlight >=0.42.0`) |
| `starlight-sidebar-topics` | 0.9.0 | 2026-09-03 | the four topics (peer `@astrojs/starlight >=0.42.0`) |
| `starlight-llms-txt` | 0.12.0 | 2026-09-15 | `llms.txt` (W13) |
| `@astrojs/check` | 0.9.12 | 2026-10-21 | `astro check` (typescript stays 5.9.3, already in the lockfile) |
| `typescript` | 5.9.3 | — | as in `web/` (stays on 5.x, `AGENTS.md`) |
| `wrangler` | 4.159.0 | 2026-11-09 | `pages deploy` (W2) |
| `playwright` | 1.65.0 | 2026-11-05 | screenshots (W12); browsers installed by hand, never in CI |
| `lychee` | 0.24.2 | 2026-05-01 | external links (W13), installed by `taiki-e/install-action` |

Astro 7 declares `@astrojs/markdown-remark` as a peer; Starlight and `starlight-openapi` peer on `@astrojs/markdown-satteri`. pnpm installs missing peers itself; if `pnpm install` warns about either, add `@astrojs/markdown-remark` 7.3.6 or `@astrojs/markdown-satteri` 0.4.7 (both published 2026-11-05). If the build asks for `sharp`, add `sharp` 0.35.5 (2026-09-27); its binaries come as optional packages and need no `allowBuilds` entry.

Newer releases existed on 2026-11-17 (Astro 7.4.12, Starlight 0.43.1, `starlight-openapi` 0.27.1) but were younger than 7 days. Dependabot moves the pins afterwards, as for `web/`.

**Workspace integration.**
- `pnpm-workspace.yaml` lists `website`. The root `pnpm install --frozen-lockfile` (`just js-install`) installs it; `minimumReleaseAge` applies as everywhere.
- Biome (root `biome.json`) checks `website/**` (TypeScript, JavaScript, JSON, CSS and the frontmatter-free parts of `.astro`), excluding `website/dist`, `website/.astro` and `website/src/generated`. Markdown is not Biome's; `check-terms.mjs` covers prose.
- Dependabot's npm entry at `/` already covers every workspace package.

**`just` recipes** (W1 unless noted):

| Recipe | Does |
|---|---|
| `docs-dev` | `pnpm --dir website run dev` (generates the public OpenAPI file first) |
| `check-docs` | `pnpm --dir website run check`: Biome, the script tests, `astro check`, the sample, term, env-var and screenshot checks, then `astro build` (links validated) |
| `docs-deploy branch` | W2: build, then `wrangler pages deploy dist --project-name cetra-site --branch <branch>` |
| `docs-gen` | W6, W7: rewrite the generated pages (CLI, SQL schema) |
| `docs-screenshots` | W12: start the web dev server on its mocks, run the Playwright script |
| `docs-links` | W13: build, then check external links with `lychee` |
| `check` | gains `check-docs` |

**CI.** `ci.yml` gains a `docs` job (checkout, `./.github/actions/js`, `just check-docs`, `just assert-clean`) in `ci-ok`'s `needs`. The deploy workflow is separate (§ Deployment).

## Deployment

- **Host:** Cloudflare Pages, project `cetra-site`, production branch `main`, custom domain `cetra.sh` (apex; the DNS is on Cloudflare, so the apex record is set by the Pages custom-domain flow). `www.cetra.sh` redirects to the apex with a Cloudflare redirect rule (a user step).
- **Workflow:** `.github/workflows/docs-deploy.yml`, on `push` to `main` and on `pull_request`:
  - one job, `deploy`, skipped unless the event is a push to `main` or a PR whose author, actor and triggering actor are the repository owner and whose head is this repository (the same gate as `CI_CACHE_ALLOWED`, since the job runs PR code with a deploy token);
  - `environment`: `docs-production` (push) or `docs-preview` (PR), each holding `CLOUDFLARE_API_TOKEN` (scope: Account › Cloudflare Pages › Edit) and `CLOUDFLARE_ACCOUNT_ID`;
  - runs `just docs-deploy main` or `just docs-deploy pr-<number>`;
  - the environment URL is `https://cetra.sh/docs/` or `https://pr-<number>.cetra-site.pages.dev/docs/`, which GitHub shows on the PR; the job also writes it to the step summary. No PR comment, so the workflow needs no write permission.
- **Previews are not indexed:** Cloudflare Pages sends `X-Robots-Tag: noindex` on preview deployments by itself; the production `robots.txt` allows everything and names the sitemap.
- **Headers** (`website/public/_headers`): `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, `X-Frame-Options: DENY`, `Permissions-Policy` with everything off, and `Cache-Control: public, max-age=31536000, immutable` on `/_astro/*` and `/pagefind/*`. No CSP in the MVP: Starlight's theme script is inline.
- **Redirects** (`website/public/_redirects`): none at first; W4 adds none either, since `docs/self-hosting/` was never published. After the first public deploy a moved page always gets a redirect (style guide rule).

## Versioning policy

- **Before the first release** (now): one version, built from `main`. A Starlight banner on every page: "cetra is not released yet. These docs describe the development version." The release that publishes 0.1.0 removes it; W13 writes that step into `website/README.md` § Releases.
- **After it:** still one version from `main`. A page or section about something merged after the latest release carries `<Since v="0.2.0" />`, which renders "Unreleased" while `0.2.0` is not a released section of `CHANGELOG.md` and "Since 0.2.0" afterwards (component written in W13, used from then on). A removed feature's page is deleted, with a redirect, in the release that removes it; its upgrade note says so.
- **Later:** if a major breaks users enough that old versions need their own docs, adopt `starlight-versions` then, through a new decision.

## Analytics, SEO and agents

- **Analytics:** Cloudflare Web Analytics on the Pages project (D-W-10). No code.
- **SEO:** `site: "https://cetra.sh"` (canonical URLs and the sitemap that Starlight generates), one `title` and `description` frontmatter per page (the description is the meta description: one sentence, under 160 characters, checked by `check-terms.mjs`), `robots.txt`, an Open Graph image (`website/public/og.png`, 1200×630, made in W1 from the wordmark on the site colours), `lang="en"`. Clean URLs with trailing slashes. No keyword pages.
- **Agents:** `llms.txt`, `llms-full.txt` and `llms-small.txt` from `starlight-llms-txt` (W13). The MCP page shows how to connect a coding agent to cetra itself.

## Screenshots

- `website/screenshots.json` lists each shot: `name`, `path` (a web UI route under the mock org and project), `waitFor` (a text or role that proves the page loaded), optional `actions` (click a row, open a tab), optional `clip` (a selector).
- `website/scripts/screenshots.mjs` (Playwright, Chromium):
  1. starts `pnpm --dir web exec vite --port 5199 --strictPort` (the `web-dev` mock mode: MSW serves `fixtures/api`);
  2. opens each route in a 1440×900 viewport at device scale factor 2, light colour scheme, `reducedMotion: "reduce"`, locale `en-US`, time zone `UTC`, the clock fixed with `page.clock.setFixedTime` at the time the fixtures are anchored to, so "5 minutes ago" never changes;
  3. waits for `waitFor` and for network idle, runs the actions, writes `website/src/assets/screenshots/<name>.png`;
  4. stops the dev server.
- Pages use them through Astro's `<Image>` (resized and served as WebP at build time) with alt text that says what the shot shows.
- `just docs-screenshots` is run by whoever changes a page shown in a screenshot; the PR includes the new PNGs. A reviewer compares them in the diff. Browsers are installed once with `pnpm --dir website exec playwright install chromium`.

## Style guide

W1 writes `website/STYLE.md` with this content (expanded with examples):

**Voice.** Second person, present tense, active voice. Plain words, short sentences, one idea per sentence. Say what to do, then why, only if the why matters to the reader. No marketing: no "seamless", "powerful", "blazing", "effortless", "simply", "just" (as a minimiser), "easy". No exclamation marks. No emoji. No performance claims (D-W-14). British spelling of "licence" (noun) as in the rest of the repository; otherwise American spelling ("normalize", "color").

**Structure.** Each page starts with one sentence that says what the page lets the reader do. Task pages are numbered steps with one action each; reference pages are tables. Headings are sentence case. Code blocks show complete, runnable code with the imports; placeholders are `<ALL_CAPS>` in angle brackets, and keys are `sk-cetra-…` with a literal ellipsis. Every Python or TypeScript block is tested or marked `{/* untested: … */}` (D-W-6). Internal links are absolute paths with a trailing slash (`/docs/tracing/sessions/`). A moved page gets a redirect once the site is public.

**Terminology** (the glossary page lists these; `check-terms.mjs` flags the wrong forms):

| Use | Not |
|---|---|
| cetra (lower case, even at the start of a sentence; `cetra` in code font for the binary) | Cetra, CETRA |
| trace, span | log, event (for a span), run (except an experiment run) |
| session | conversation, thread (as the product noun) |
| user (the end user of the reader's app) | customer, end-user |
| member (a person in an org) | user (for people who sign in to cetra) |
| organization, org | team, workspace, tenant |
| project | app, environment (for a project) |
| environment (the `environment` field) | stage |
| score | grade, metric (for a score) |
| evaluator; LLM judge | eval (as a noun), grader |
| online rule | monitor, automation |
| dataset, dataset item | test set, example |
| experiment | eval run |
| API key (`sk-cetra-…`), public key (`pk-cetra-…`), personal access token (`pat-cetra-…`) | token (alone), secret |
| self-hosted; cetra Cloud (once it exists) | on-prem, SaaS |
| the UI | the dashboard (except the Dashboard page), the app |
| OpenTelemetry (prose), OTel (only after the first use on a page) , OTLP (the protocol) | otel, Open Telemetry |
| `llmobs` never appears (the old working name, D46) | |

## What the user does by hand

Agents cannot do these; each phase that needs one says so and stops at it.

1. **W2:** create the Pages project (`wrangler pages project create cetra-site --production-branch main`, or in the dashboard as a "Direct Upload" project); create the API token (Account › Cloudflare Pages › Edit); create the GitHub environments `docs-production` (deployment branch `main` only) and `docs-preview`, each with `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
2. **W2, after the first production deploy:** attach `cetra.sh` as the project's custom domain, which replaces the record that serves the holding page today; add the `www` redirect rule; turn on Web Analytics for the project.
3. **W12:** run `pnpm --dir website exec playwright install chromium` once on the machine that takes screenshots.

## Open questions

1. **Apex move (D-W-2).** Where is the holding page served from today? Moving `cetra.sh` to the Pages project in W2 replaces it with the same page. If the apex must stay where it is for now, the fallback is to serve the docs at `docs.cetra.sh` until the marketing site is ready, at the cost of a host change (and redirects) later.
2. **Implementer model for content.** Tooling phases (W1, W2, W4's tests, W5, W6, W12) are mechanical. Content phases (W3, W7–W11, W13) are mostly writing from specs; the plan gives each page's scope, sources and required elements, but prose quality will follow the model. Run those with Sonnet instead of Haiku?
3. **Cloud.** The docs assume self-hosted only (`base_url` defaults to `http://localhost:8080`). When track Q opens signups, the quick start gains a "cetra Cloud" tab; that is a later phase, not part of W.
