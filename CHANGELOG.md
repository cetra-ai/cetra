# Changelog

All notable changes to cetra are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (https://cetra.sh/docs/self-hosting/upgrade/).

## [Unreleased]

### Added

- Calls through the OpenAI client show the provider read from the endpoint's host when the instrumentation records it: cetra's Python SDK with `instrument=["openai"]` (which now records the client's host), OpenLLMetry, and OpenInference for TypeScript. This covers OpenRouter, Groq, Novita, Together, Fireworks, DeepSeek, xAI, Databricks and the other hosts in the host table.
- Integration pages for 35 model providers and aisuite, with captured spans for Groq, Mistral AI, Cohere, Together AI, Ollama, Azure OpenAI and Vertex AI; `cetra-sdk` extras `groq`, `mistralai`, `cohere`, `ollama` and `together`.
- Integration pages for Instructor, Agno, smolagents, Haystack, BeeAI Framework, Guardrails AI, Deep Agents, Temporal, LangGraph and the Claude Agent SDK, with captured spans for nine of them (seven new captures; LangGraph and the Claude Agent SDK use existing ones); `cetra-sdk` extras `instructor`, `agno`, `smolagents`, `haystack` and `beeai`.
- Integration pages for Pydantic AI, Semantic Kernel, Strands Agents, Bedrock AgentCore, Microsoft Agent Framework, AG2, AutoGen, Mirascope, OpenHands, Mastra and VoltAgent, with captured spans for ten of them (OpenHands's case is hand-written), and the `voltagent` convention for VoltAgent's spans. AutoGen is `instrument=["autogen-agentchat"]`, with `openinference-instrumentation-autogen-agentchat` installed separately.
- Integration pages for the gateways: LiteLLM Proxy, Bifrost, Envoy AI Gateway, Kong AI Gateway, Portkey, TrueFoundry, Vercel AI Gateway, Helicone and Cloudflare AI Gateway, with captured spans for LiteLLM Proxy, Bifrost, Envoy AI Gateway and Temporal, and `just capture-tools` to fetch the pinned binaries those captures start.
- Integration pages for n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, MCP, OpenLIT, the vector databases (Chroma, Qdrant, Milvus, Pinecone, Weaviate), Promptfoo, Stagehand, LobeChat, Gradio, Ragas, UQLM, Azure AI Evaluation, Cleanlab, and Harbor and Terminal-Bench, with captured spans for ten of them (eleven new captures); `instrument=["mcp"]` and the `cetra-sdk[mcp]` extra carry an MCP server's spans into the client's trace.
- The capture harness is its own package (`tools/capture/harness/`), and capture scenarios whose packages cannot share the capture project's lock live under `tools/capture/envs/<env>/scenarios/`; `just capture-tools` installs every environment.
- Vector database reads are `retriever` spans: OpenLIT's `vectordb` get, query and search, and the read spans of OpenLLMetry's Chroma, Qdrant, Milvus, Pinecone and Weaviate instrumentations. Writes stay `span`.
- Qwen Code, Pi and OpenClaw are coding agents: `cetra agents setup qwen-code`, `cetra agents setup pi` and `cetra agents setup openclaw` connect them to cetra, and `cetra agents remove` undoes each. Pi gets an extension that cetra writes, and OpenClaw's setup installs its diagnostics plugin. Integration pages cover these three and Augment Code, Devin SDK and Omnigent (setup only).
- OpenCode, Goose and Oh My Pi are coding agents: `cetra agents setup opencode` (which writes cetra's OpenCode plugin and its config file), `cetra agents setup goose` (which prints Goose's environment lines) and `cetra agents setup oh-my-pi` (which writes Oh My Pi's `.env` lines) connect them to cetra, and `cetra agents remove` undoes each. Integration pages cover these three and Hermes Agent, which is detected and captured but has no setup.
- Integration pages for Spring AI, LangChain4j, Quarkus LangChain4j, Koog and Microsoft.Extensions.AI, and .NET sections on the Semantic Kernel and Agent Framework pages, with captured spans for all of them: the capture tool runs Java and .NET scenarios, built by `just capture-tools` from the pinned JDK, Maven and .NET SDK. Setup-only pages cover Genkit Go, openai-go, ruby_llm, rig and Arconia.
- Integration pages for eve, Agent Spec, Atomic Agents, TanStack AI, Flue and EverOS, with captured spans for all of them, a Claude Managed Agents section on the Anthropic page and a LangServe note on the LangChain page. Setup-only pages cover Cloudflare Agents, Graphite, the NVIDIA NeMo Agent Toolkit, Pixeltable and Cognee.
- Audit records: sign-ins, membership, invitation, credential, SSO, licence and deletion actions, configuration changes of evaluators, rules, alerts, connections, prices, prompts, datasets, issue sources, dashboards and annotation queues, and data leaving the product (SQL runs, exports and MCP tool calls) are recorded in the metadata database with the actor, IP and user agent. Records are kept for `audit.retention_days` (default 365). Reading them comes with the enterprise audit log.
- SCIM 2.0 user provisioning (Enterprise and the self-hosted licence): per-org tokens, Okta and Entra ID compatible Users endpoint, deprovisioning that revokes access, and members managed by the identity provider.
- SCIM groups and role mappings to org and project roles, with Okta and Entra ID setup guides.
- Audit log pages and API for organization admins and instance admins, with filters and CSV/NDJSON export (Enterprise and the self-hosted licence).
- Audit log streaming to an HTTPS endpoint (Splunk, Datadog or any NDJSON receiver), signed, with retries.
- The organization home shows a card per project with its last 24 hours (traces, error rate, LLM cost, an hourly chart with errors, the last ingest time), a cards/table switch, a search box, and this month's usage against the plan. `GET /api/v1/orgs/{org}/projects/activity` serves the cards.
- Org owners can set up SAML 2.0 single sign-on for their org, with metadata import, automatic certificate rotation from a metadata URL, and the same domains and enforcement as OIDC (the `saml` licence feature).
- LLM connections: **Test connection** checks the form's values before you save them, for a new connection too, and **Test embeddings** under Advanced checks an embedding model. The panel header shows when the saved values were last tested. `POST /api/v1/orgs/{org}/llm-connections/test` tests an unsaved connection, and `POST …/llm-connections/{id}/test` takes draft fields; a connection's `last_tested_at` and `last_test_ok` hold the last test of its saved values.
- The connection form suggests common model ids for OpenAI, Anthropic and Gemini in the Default model field.
- Media parts carry an optional `transcript`. OpenAI chat output audio keeps its transcript, a message with only audio previews its transcripts, and judges read `[audio: <transcript>]`.
- The alert editor, the rule panel and the rule list chart an issue occurrence rule's occurrences per window with its threshold, through `GET /api/v1/projects/{project}/alert-rules/issue-preview`.
- The playground compares versions of a prompt side by side: each column of a draft opened from a prompt can run one of its versions, with the version's template, answer, tokens, cost and latency; `?compare=` opens two versions.
- The trace list shows the trace count, error count and cost of the checked rows, or of every trace the list matches, under its table; `GET /api/v1/projects/{project}/traces/totals` answers the same totals for a filter, search and time range.

### Changed

- Tool calls in the Chat tab and the trace Transcript are one line each, `name(arguments) → result · duration`, with the tool's response folded into its call and a failed call's error shown on the line; select a line to open the arguments and result. The Transcript has a **Full width** toggle that hides the panel beside it.
- The install command is `curl -fsSL https://cetra.sh/install.sh | sh`: `cetra.sh/install.sh` redirects to the release's install script. The GitHub release URL keeps working.
- Chart bars have slightly rounded tops; in a stack only the top segment is rounded.
- Span kind icons use the Tabler icon set (a robot for agents, a wrench for tools, …), slightly larger.
- The trace tree draws elbow lines between parents and children, its Duration, Tokens and Cost columns fit their values, and up to three slow spans (the largest self times above 20% of the trace) show their duration in amber. The timeline drops the metric columns to give the bars the full width: taller bars with the duration and cost inside them, round axis ticks with gridlines, and labels kept inside the panel.
- Dark Reader and the browsers' automatic dark mode no longer darken the web UI, which has its own dark theme.
- `PATCH /api/v1/orgs/{org}/llm-connections/{id}` no longer needs `api_key` with a new `base_url` or `adapter`: the saved key is kept.
- The LLM connection form offers OpenAI, OpenAI-compatible, Anthropic and Gemini; a saved OpenAI connection with a base URL shows as OpenAI-compatible, and `GET /api/v1/projects/{project}/assistant` returns the connection's `base_url`. The form puts the base URL and structured output under Advanced unless the provider needs them, shows the saved key's last 4 characters, and its fields keep a readable width. The Test dialog is gone.
- The Python and TypeScript SDKs send `User-Agent: cetra-sdk-python/<version>` and `cetra-sdk-js/<version>` on their requests; Python's trace exporter adds it after the OpenTelemetry exporter's own value.
- Strands time to first token is read in milliseconds; framework names written as the provider (Strands, Microsoft Agent Framework, AutoGen, Spring AI) are skipped; Microsoft Agent Framework workflow steps and Mirascope trace spans are chain spans.
- LiteLLM Proxy's and Bifrost's own cost figures are read as the reported cost; Bifrost's server root span is a chain, not a second LLM call; `gen_ai.operation.name` values `embedding` and `aembedding` (LiteLLM's call types) give the embedding kind.
- OpenLLMetry completions without a role are read as assistant messages; a `function_call` is a tool call.
- Bifrost's server root span carries plain text instead of messages in its message fields; cetra stores it as text and records no normalize error.
- Scope-gated rules read one instrumentation's keys on its own spans only: Prompt Flow (`span_type`, `inputs`, `output`, `llm.response.model`, `llm.usage.*`), Langflow (`trace_type`, `inputs`, `outputs`), n8n (span names), LiveKit Agents (`lk.pii.*` content and the provider host), Pipecat (scoped `input`/`output` and `conversation.id`), OpenLIT (`gen_ai.operation.type`), Promptfoo (`gen_ai.evaluation.name`) and the vector databases.
- `tts` and `stt` operations (Pipecat's speech spans) are plain spans, and a host in LiveKit's `gen_ai.provider.name` names the endpoint: `api.openai.com` gives the provider `openai`.
- A reported cost of exactly 0 on a span that counts tokens counts as no report and is priced from the price table; OpenLIT writes a cost of 0 when its price table has no row for the model.
- `/v1/agent-context` accepts `qwen-code`.
- Semantic Kernel .NET's GenAI events carry their message in `gen_ai.event.content`, which is read as the event's body; a GenAI message value that starts with `HIDDEN:` (Koog's masked content) is content not captured, not a malformed message; `koog` is a framework name, `open_ai` is an alias of `openai`, and on scope `io.quarkus.opentelemetry` `gen_ai.prompt` and `gen_ai.completion` fill input and output.
- Spans of Vercel's Workflow SDK (scope `workflow`, names starting `workflow.`, `step.` or `queue.`), which eve runs on, are discarded and counted, as coding agents' internal spans are, unless they carry model attributes (`gen_ai.*`, `llm.*` or `openinference.span.kind`); on Agent Spec's `LlmGenerationSpan` model calls, a token attribute whose value is the string `"null"` is absent, with no warning.
- `@cetra-ai/sdk`'s Anthropic helper needs `@arizeai/openinference-instrumentation-anthropic` 1.0.1 or later, which keeps the Anthropic SDK's own span out of a traced call, so its tokens count once.
- The session page shows each turn's tool calls and, on demand, its model and tool calls in order with a preview of their input; turns can be searched, filtered (failed, scored, uses tools) and shown one per line; an overview strip, `j`/`k` and the errors chip move between turns.
- The trace view plays audio in a themed player with a waveform and shows its transcript under it.

### Fixed

- When an org has its own single sign-on, the login page's email step links to the password form ("Sign in with a password instead"), so instance admins and members of a domain whose sign-on is not enforced can still use their password. After an org's provider refuses to sign in an instance admin, the page shows the password form.
- The top bar's search button no longer runs its label into the `Ctrl+K` shortcut on Linux and Windows.
- OpenRouter, Vercel AI Gateway, Helicone, Portkey and Cloudflare AI Gateway spans are priced by the model publisher's price, and the built-in prices include Groq, Novita, Nebius, DeepInfra, Baseten, Hyperbolic and Together models that were missing.
- OpenInference outputs labelled as JSON that hold plain text, as Agno's do, no longer record a `cetra.normalize_error` attribute.
- A Laminar `LLM` span without a model or token counts (such as the OpenHands SDK's wrapper) is now a chain span, so its reported cost is not counted twice.
- Selecting another member while the member panel is open no longer adds a second Role card.
- A model named with a provider prefix, such as `openai/gpt-4o-mini` from LiteLLM-style clients and EverOS, is priced as the provider's model when no price row matches the full name.
- An online backup cancelled by a server shutdown no longer leaves files in its directory: the shutdown waits until the backup has stopped and removed what it wrote.
- A session turn's score badges wrap instead of overlapping each other or the turn's cost, also in an annotation queue's narrow review column. The trace header puts its actions on a second line when they don't fit beside the name. The playground's prompt version select shows a long label with an ellipsis and in full on hover.

## [0.1.0-rc.2] - 2026-11-22

### Added

- Instance admins install a licence key from the admin settings, and it applies without a restart. A key in `CETRA_LICENSE_KEY` still wins.
- The licence card links to the licence store to buy or renew a licence.
- `CETRA_INIT_READ_KEY` creates a read-only API key of the first project, with a value you choose,
  when the installation first starts, for example to read traces in a restore drill.
- `npx @cetra-ai/cli init` sets an app up for cetra in one command and prints a prompt for your coding agent; `cetra-cli query` checks the traces from the terminal; the cetra agent skill is installed in `.claude/skills/cetra/` and `.agents/skills/cetra/`.
- Every docs page has a Markdown copy at `<page>/index.md`, with Copy page and View as Markdown actions; `https://cetra.sh/mcp` is a docs MCP server for coding agents (`search_docs`, `get_page`).
- `POST /api/internal/v1/device/codes` lets a control plane register a device code on a cell (cloud sign-in).

### Changed

- `cetra login` signs in with a code you approve in the browser, so it works over SSH and in containers.

### Removed

- The `/cli-auth` page and the CLI's localhost callback.

### Fixed

- Shutdown closes `catalog.db` before it releases the data directory lock, so the SQLite
  checkpoint no longer writes the file after the server has stopped.
- Compaction no longer fails on every pass for a partition of large spans (LLM prompts and
  completions): a merge reads fewer rows per batch when its rows are large, so its batches fit
  `compaction.max_memory`, and days close.
- `url` 2.5.9 fixes a stack overflow on nested `blob:` URLs (RUSTSEC-2026-0592).

## [0.1.0-rc.1] - 2026-11-21

### Upgrade notes

- The control plane's `users/revoke`, `users/disable` and `users/delete` calls must now send
  `email` with `subject`; a body without it is refused with 400.
- Migration `0015` adds `users.email_verified_via` and `web_sessions.via_provider`. Verified accounts
  that exist already have no recorded origin and count as unproven: when an identity provider links
  one of them, its password is cleared. With `auth.enforce_sso_domains`, sessions that predate the
  upgrade stop resolving for the listed domains, and those users sign in again through their provider.
- The cloud control plane must sign `method` and `path` claims in its `control_plane` tokens; a token
  without them is refused (D-P-71).
- With `auth.signup = open`, an email alert channel that lists an address which is not a verified,
  enabled member of its org is refused on its next edit, and its deliveries skip that address and are
  recorded as failed. Change the recipients, or use `invite_only` signup.
- A trace `latency_p95` alert rule whose threshold is at or above its window is refused on its next
  edit; lower the threshold or widen the window.
- A`CETRA_AUTH__OIDC__*` environment variable other than
  `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET` for a provider of the config file now stops the
  server at start; such variables used to be ignored. Remove them.
- `/readyz` can now fail on a full disk or an unreachable object store. Keep liveness probes on
  `/healthz`; the Helm chart already does.
- `/healthz` now fails on a storage failure that a restart clears (a WAL, object fsync or catalog
  failure), so a liveness probe on it restarts the server.
- Migration `0030` rebuilds `notification_deliveries` (an issue notification is a delivery without a rule event) and adds `issue_sources.last_notified_at`.

### Added

- The Python SDK is published to PyPI as `cetra-sdk` and the TypeScript SDK to npm as
  `@cetra-ai/sdk`, with the server's version, from each release tag (D-N-37).
- `alerts.email_recipients` (`"members"` or `"any"`) limits email alert channels and their
  test sends to the org's verified members whatever `auth.signup` is (D-K-21). Unset, it keeps the
  old rule: members only under open signup.
- Alert rules on issues: the `issue_occurrences` metric counts the occurrences of one issue or of a source's issues in a window of 15 minutes or more (D-IC-20).
- Issue notifications: an issue source can send its new and regressed issues to project channels, in one message per 10 minutes at most (D-IC-21).
- Issues: "Add examples to dataset" copies an issue's newest occurrence traces into a dataset, and "Create alert" opens an alert rule for the issue (D-IC-22, D-IC-23).
- Issues: "Send to queue" adds an issue's newest occurrences (traces, spans or sessions, by the queue's kind) to an annotation queue (D-IC-27 to D-IC-31).
- MCP: the read-only `list_issues` tool lists a project's issues with their state and counts over the last 7 days (D-IC-24).
- Issue sources for bad scores, user feedback and topics: LLM summaries and embeddings group them into issues, refreshed every 6 hours (D-IC-4, D-IC-8).
- MCP tools `list_prompts` and `get_prompt` (D-V-39): coding agents read the project's prompts, their labels and a version's template, config and variables, with the same access and limits as the other MCP tools.
- Prompts pages (D-V-32): the prompt list with search, label and tag filters, protected labels and a create dialog; the prompt page with the template editor (saving a version), versions with labels and line diffs, label history and per-version metrics; prompt links, facets and columns on traces and spans.
- In-app assistant (D193, D-AS-1 to D-AS-31): a chat panel in each project (`Ctrl/Cmd+I`) that answers questions over traces, metrics, evaluations and settings with the organization's LLM connection, and drafts filters, SQL, dashboards, evaluators and online rules that you apply.
- Organization setting Assistant (D-AS-3, D-AS-24): off until an admin picks a connection and a model; turning it on runs a tool-calling test.
- Assistant API (D-AS-9, D-AS-10): conversations, turns streamed over SSE, feedback, and the org setting under `/api/v1/…/assistant`.
- Annotation queues in the UI: create queues, add traces, spans or sessions from their pages and lists, and review them one at a time with Complete and next.
- Add everything a trace, span or session filter matches to an annotation queue, up to 10 000 per call: `POST /annotation-queues/{id}/items/matching/preview` counts the matches and `POST /annotation-queues/{id}/items/matching` adds them.
- Annotation queues on the server: queues of traces, spans or sessions with their score definitions and instructions, items added by id, a 5-minute lease with next, complete, skip and reopen, the GDPR and retention cascade, and the `max_annotation_queues` plan limit (Free: 1).
- `Client.annotation_queues` (Python) and `client.annotationQueues` (TypeScript): create annotation queues and add traces, spans or sessions to them.
- Scores carry an optional `queue_id`, the annotation queue they were written from, and `GET /scores` filters by it.
- Playground API (D-V-20 to D-V-26, D-V-33 to D-V-37): POST /playground/runs streams a model run over SSE through the org's LLM connection, traces it into the reserved cetra-playground environment (not billed), with per-user and per-org limits; cetra-playground is reserved everywhere cetra-eval is.
- Prompt links on spans and traces (D-V-15, D-V-16): `cetra.prompt.name`/`version` and Langfuse's prompt attributes fill `prompt_name`, `prompt_version` and the traces' `prompts` list (filter with `prompts @> ['name@version']`); `GET /prompts/{name}/metrics` gives per-version cost, latency, errors and scores (D-V-17). Spans are now normalized with `schema_version` 2.
- `cetra agents setup` and `cetra agents remove` connect Claude Code, Codex, Gemini CLI and Copilot to cetra, with a hook that reports each session's repository, branch and person.
- Claude Code, Codex CLI, Gemini CLI and GitHub Copilot spans are normalized: tool spans are named after their tool, tokens and cost are filled, and traces carry the agent, repository, branch and person.
- Dashboard templates — Cost, Latency and Quality — in the New dashboard dialog; the Overview adds top users and top trace names by cost and drops its section headings (D-C-21).
- A demo org that every signed-in user can read (`demo.org`): users see it last in their org switcher as an implicit viewer, without a membership row (D187).
- SDK prompts (D-V-14, D-V-15): get_prompt / getPrompt with a 60 s cache, stale-while-revalidate and a fallback, Prompt.compile, prompt links through context(prompt=…) / withContext({ prompt }), and client.prompts.
- Prompt registry API (D191, D-V-2 to D-V-13): versioned text and chat prompts with plain {{var}} templates, labels with protected labels, label history, and a fetch with ETag at GET /prompts/{name}/resolve.
- A filter sidebar on the Traces, Spans, Sessions and Users lists: per-field values with counts, keys of metadata and attributes, and histograms with range selection for durations, costs and tokens, all written into the filter text. Backed by the new `POST /facets` endpoint (D-C-13).
- Preset filters (Errors, Slow, Expensive, Has score, LLM), value autocomplete in the filter editor, a "+ Filter" button with a searchable field picker and token input for `IN`, and wrapping chips in a new filter row.
- Lists opened without URL parameters keep the range, flags and compatible filters of the last list (D-C-14).
- A right-click menu on filterable list cells: filter, exclude, copy, open the session or user.
- Dashboard: KPI sparklines, a shared hover cursor across charts, drag to zoom the range, per-score small charts, and top-model rows that open the filtered traces (D-C-15).
- Custom dashboards (D-C-16): any number of named dashboards per project on a 60-column grid (D-C-19) that members edit by dragging, resizing and a widget editor; widgets are line, bar, area, horizontal bar, number and table charts, and headings. The built-in Overview is a read-only preset of the same widgets that can be duplicated. API: `GET/POST /dashboards`, `GET/PATCH/DELETE /dashboards/{id}` and `POST /dashboards/query`, which computes one widget from traces, spans or scores with up to six measures, a breakdown and filters.
- The Overview is editable in place, with "Reset to default" (D-C-18): the first save stores the project's own copy, and everyone sees it. Charts on any dashboard a member can edit are dragged and resized without entering Edit; the layout saves itself, with an Undo bar.
- Online rules: `GET /online-rules/stats` answers the last 24 hours per rule, with the finished jobs by hour and the score summary with an hourly series (D-I-50), and the reworked Online rules page shows them beside a full-width list with a side panel: status, spend, hourly jobs, score summary and trend, and recent failures.
- Experiments:
  - The list is full width, with a status strip, metadata tags, score badges with the change since the previous run, errors, latency and cost, and a trend chart per dataset. Experiments open in the list's panel (drawer, bottom or page).
  - Experiment pages show score distributions and open each item in a side sheet that renders the input and outputs as Markdown or JSON.
  - The compare view filters items that are regressed, improved or changed, counts improved and regressed items per score, and opens an item with its outputs side by side.
  - New: `GET /experiments/summaries?ids=` and `ScoreSummary.histogram` (D-J-20, D-J-21).
- Users: the user page has a time range and Overview, Sessions and Traces tabs; Overview shows the user's scores, metadata, and the dashboard's KPIs and charts for that user, with a link to the dashboard filtered to them. The list shows each user's name and plan from `user.*` metadata, relative first/last seen, KPI changes against the previous period and the top users by cost; the drawer shows metric chips, an activity chart and each recent session's first message. `GET /users/{user_id}` takes `from` and `to`.
- Licences: release builds also accept licence keys signed by `license-1`, the cloud control plane's online licence key, and `license-2`, its offline spare (D-P-73).
- OTLP: `POST /v1/logs` and `POST /v1/metrics` (HTTP) and the gRPC `LogsService` and `MetricsService` are accepted and dropped instead of answering 404 (D-G-40). GenAI content sent as log events is counted per project; `GET /api/v1/projects/{project}` returns it as `genai_log_events`, and the UI tells the project's users once that the content was dropped. Migration `0018` adds `usage_daily.genai_log_events`.
- Alerts: a `no_data` event kind, shown in the history, and a "No data" notification on Slack, email and webhooks (`alert.no_data`) (D-K-20).
- Datasets: versioned datasets of input / expected output / metadata items with optional JSON Schemas, and their API (`/datasets`, items, versions) (track J).
- Datasets: import items from CSV (with a column mapping) or JSONL, export a version as JSONL or CSV, and add an item from a trace or a span (track J).
- Experiments API: create an experiment from a dataset version, store results and finish it; results with score summaries and a 2–4 experiment comparison; experiment scores; `dataset_items` and `experiment_items` in the SQL console and MCP; deleting a trace also deletes dataset items copied from it and experiment results about it (track J).
- Python SDK: `cetra.Client` for datasets and `cetra.evaluate` / `aevaluate` to run experiments on your machine (track J).
- TypeScript SDK: `new cetra.Client()` for datasets and `cetra.evaluate` to run experiments on your machine (track J).
- Experiments: the experiments list, experiment page and compare view (2–4 experiments, deltas, regressions and a word-level output diff) in the web UI (track J).
- Datasets: the dataset pages in the web UI, with items, versions, import, export and an Experiments tab (track J).
- Datasets: item search (`q`), batch edit and batch delete of items in one version each, a version's added, edited and deleted counts with its item changes, restore of an earlier version, bulk add of up to 100 traces, and `GET /dataset-trends` with the latest experiments and score means per dataset (track J, D-J-22 to D-J-26).
- Datasets UI: a full-width list with a summary strip, last experiment and score trend columns and a dataset panel; a dataset page with header chips, a "Run experiment" snippet, an items grid with previews, search, an item panel, cell edit mode and bulk delete; a versions timeline with change counts, diffs and restore; how-to empty states; and "Add N to dataset" for checked rows of the traces list.
- Settings → Model prices, for an organization and for a project: add, edit and delete custom prices, search the built-in prices, and check which price applies to a model.
- Custom model prices per project: `/api/v1/projects/{project}/models` lists the effective price table, resolves which price applies, and adds, replaces and deletes project prices. New prices apply to spans received afterwards; stored costs are not recomputed (D-T-5). Org-wide prices under `/api/v1/orgs/{org}/models` apply to every project of the org; a project price beats an org price.
- Auth: an `[[auth.oidc]]` provider's client secret can come from
  `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`, and `client_secret` may then be left out of
  `cetra.toml` (D-S-2).
- Storage: a failed fsync of a local object stops local object writes until a restart, and `StorageHealth::objects_sync_failed` reports it (D-R-6). New error `ObjectError::SyncFailed`.
- Storage: a `catalog.db` commit that fails with an I/O error stops the catalog until a restart, and `StorageHealth::catalog_failed` reports it (D-R-6). New errors `CatalogError::Stopped` and `CommitError::Stopped`.
- The S3 self-hosting guide recommends a lifecycle rule that aborts incomplete multipart uploads (track R).
- Storage: a warning at start when the storage's memory budgets add up to more than the memory limit.
- Per-org single sign-on (enterprise): an org's own OIDC provider, DNS-verified email domains,
  email-first login, just-in-time membership and SSO enforcement; `cetra admin org-sso disable`.
- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
- Plans with a data-access window hide older traces, spans, scores and media from every read
  (UI, API, SQL, MCP, alerts, evaluators); the time-range picker shows the limit.
- Control-plane org deletion (`DELETE /api/internal/v1/orgs/{org_id}`) and an applied-drops feed
  (`GET /api/internal/v1/deletions`) for cloud cells.
- `email_verified` and `has_password` on the `User` returned by `GET /api/auth/me`, signup and
  setup.
- `GET /api/v1/projects/{project}/saved-views/{id}` returns one saved view by id.
- `cetra admin backup` takes a consistent backup of a running server (local mode: hard-linked
  objects; S3 mode: metadata, restorable for `storage.gc.grace`, which may now be up to 30 days).
- `cetra admin gc-orphans` cleans the object store after a restore and reports missing objects.
- `cetra admin migrate` migrates `meta.db` and `catalog.db` while the server is stopped, before a
  restore's deletions are re-applied.
- The server keeps a copy of `meta.db` and `catalog.db` before migrating them.
- `meta.db` records its format version. `cetra serve`, `admin migrate` and `admin gc-orphans`
  refuse a `meta.db` written by a newer release before they open storage or change any file.
- The admin socket (`admin.sock`) carries a protocol version; a CLI and a server of different
  protocol versions refuse each other with a message naming both.
- A backup's `manifest.json` records the formats and last migrations of `meta.db` and `catalog.db`
  (`meta_format`, `meta_schema`, `storage_format`, `catalog_schema`).
- Helm chart values `extraVolumes` and `extraVolumeMounts` (for example a backup volume).
- Restore mode (`restore_mode`): the server serves reads and makes no outbound call, for restore drills.
- Prometheus metrics on a separate listener (`metrics_addr`); the Helm chart's `metrics.enabled`.
- A layout switcher on the Traces and Spans lists: split, drawer, rail and bottom.

- Evaluator version history: every change to an evaluator's config or output is kept, `GET /api/v1/projects/{project}/evaluators/{id}/versions` lists the versions, and the evaluator page has a History tab that diffs a version against the previous one.
- Evaluators: `GET /evaluators/stats`; the evaluator list shows 7-day runs, errors, average cost, p50 and result columns (the score column is now "Output"), and the evaluator page a 7-day stats row and the experiments with its score (D-I-48).
- Issues: error spans of production traces are grouped into issues you can resolve, ignore and merge, with occurrence counts and a filter on the trace list (D-IC-2).
- Playground (D-V-20 to D-V-28, D-V-38): run prompt versions and recorded LLM spans against up to four models side by side, streamed, with cost and latency; save the result as a prompt version; recent runs.
- A "Coding agents" dashboard template, and `session_id` as a breakdown of trace widgets.

### Changed

- `GET /facets` is replaced by `POST /facets`, which answers up to 20 fields of a list in one request, each with its own result or error; the facet sidebar and presets send one request when a list opens without a filter or search (D-C-24).
- Traces, Spans, Sessions and Users come back from cache within 15 minutes, with the same rows and
  scroll position; newer rows wait behind an "N new … ↑" pill, and a new filter or range keeps the
  previous rows dimmed until the answer arrives.
- The self-hosting guides moved from `docs/self-hosting/` to cetra.sh/docs/self-hosting/.
- The SDK guides moved from the package READMEs to cetra.sh/docs/reference/python/ and
  cetra.sh/docs/reference/typescript/; the READMEs keep install and the quick start.
- The copyright holder in the MIT `LICENSE` files is sebastiencs, which is also named as the licensor of the Elastic License 2.0 directories (D190).
- The top bar is a breadcrumb to the current page and its open item, shows whether spans are arriving, and lists firing alerts (D-C-20).
- Dashboard charts use round axis ticks, at most 60 buckets, the error colour for errors and one colour per model; change badges show `new` or `×N` instead of huge percentages. The list toolbars' Eval and Experiments switches are now one "Include" menu (D-C-15).
- The project sidebar groups its pages into Observe, Monitor, Evaluate and Explore, with live badges on Alerts, Online rules and Experiments, a Recent group, and the account menu, Docs link and version in its footer; `g` then a letter now reaches every project page (Spans moves to `g p`, Sessions to `g s`), and the sidebar starts collapsed below 1024 px (D-C-12).
- Settings: `/settings` is now an overview with a live status per section; every settings page shares one layout with per-card Save, a danger zone last, inline validation and an unsaved-changes prompt; lists (API keys, members, invites, tokens, LLM connections, channels, model prices, score definitions) get search, sort, status pills and a row panel; destructive actions ask for the item's name; the settings sidebar has a filter and a usage meter, and ⌘K lists every settings page.
- `GET /experiments/compare` takes `only=regressed|improved|changed` in place of `regressions_only`; compare cells carry `improved`, and score deltas count the improved and regressed items (D-J-21).
- New logo, the Caetra c (D189): the web UI's favicons, and the brand files in `assets/brand/`.
- The product is renamed from the working name `llmobs` to cetra (cetra.sh, D46): the binary and crates,
  the `CETRA_*` environment variables, the `cetra.*` span attributes, the `sk-cetra-`, `pk-cetra-` and
  `pat-cetra-` key prefixes, the `ghcr.io/cetra-ai/cetra` image, the `cetra` Helm chart, and the
  SDK packages (`cetra-sdk` on PyPI, imported as `cetra`, and `@cetra-ai/sdk` on npm). Update config files, environment
  variables and SDK code to the new names.
- Auth: with `auth.disable_password = true`, instance admins can still sign in with their
  password at `/login?password=1`, change it, and set it from a reset link
  (`cetra admin reset-password`) (D-S-3).
- Deletion, project and organization drop dialogs no longer promise erasure within 24 hours: data is hidden within seconds, removed within 24 hours, and its stored copies are purged after the storage grace period (`storage.gc.grace`, D181).
- Alert email is capped at 1,000 messages per org per UTC day, "Send test" is limited to 5 a minute
  per user and per channel, and when signup is open an email channel may only list verified
  members of its org (D-K-19).
- LLM latency alerts window on the span's end time, so a long call that ends in the window is
  counted; a trace latency threshold at or above the window is refused with 400 (D-K-18).
- `settings` is a reserved project slug (400 on `slug`). `retention_days` on project create and
  update is nullable in the OpenAPI contract, from 1 to 36500; `null` keeps data forever.
- A read that makes several storage calls (a list with scores, a session or user detail) has 30 s
  in total, not 30 s per call.
- `cetra login` receives the token from the browser as a form POST, not in the callback URL, so
  the token stays out of browser history. A token created by the sign-in page expires after 90
  days; run `cetra login` again when it does.
- `PATCH /api/v1/orgs/{org}/llm-connections/{id}` that changes `base_url` or `adapter` must send
  `api_key` again (400 on `api_key`), so a new host never receives the stored key.
- Web UI: with password sign-in off and exactly one sign-in provider, the login page and, for a
  visitor without a session, the invite page go straight to that provider; signing out lands on a
  page that does not. Instance admins sign in with their password at `/login?password=1`, which the
  login page links to (D-S-4, D-S-5, D-S-6).
- Internal API: `POST /api/internal/v1/users/{revoke,disable,delete}` take `{subject, email}` and
  find a user who never signed in through the control plane by email (D-S-9).
- The Alerts, Issues, Prompts, Playground, Evaluators, Online rules, Issue sources, Annotation
  queues, Datasets and Experiments pages share one toolbar layout and table style. Experiments has
  no Total chip and Evaluators no All button: select the selected chip again to clear the filter.
- Item pages (new and edit forms, and the evaluator, issue, issue source, dataset and annotation queue pages) use the list pages' toolbar and left-aligned layout. The alert rule editor gets a back link.

### Fixed

- Alerts: a firing rule whose data stops (two readings below `min_count`) no longer reports itself resolved; it moves to `no_data` and sends a no data notification, and resolves when a reading with data is clear (D-K-20).
- A WAL record with a corrupt compressed-buffer length is rejected as corrupt instead of aborting the server at startup.
- The WAL now rejects a batch whose decoded size exceeds 1 GiB with an invalid-batch error (previously such a record could not be replayed).
- A Parquet decoder panic on a corrupt data file no longer drops the request, stops compaction or
  prevents startup: the read fails, and the file is verified and quarantined (track R).
- Web UI: the sidebar opens on screens under 768 px (a trigger above the page); session ids show in full in the session header; the traces list refreshes after "Delete matching…" once storage hides the rows, and the deletion history says its trace count includes evaluation traces; Enter submits the Save view and Rename view dialogs.
- Security review of the auth code: an invitation no longer counts as proof of an email address, so
  linking Google, GitHub or an org's identity provider clears the password of an account made with an
  invitation (D-P-66).
- API keys can no longer list the org's members through `GET /projects/{project}/members` (D-P-67).
- An instance admin cannot mint a reset link for the owner of an org they are not a member of (D-P-68).
- `auth.enforce_sso_domains` now also ends password sessions, personal access tokens and password
  resets of the listed domains (D-P-69).
- Dropping a project or an org needs a browser session; a personal access token gets 403 (D-P-70).
- Control-plane tokens are valid for one method and path only (D-P-71).
- Org SSO domains are re-checked daily; a domain whose TXT record is gone stops being verified
  (D-P-72).
- Auth security review (`docs/research/auth/review-2026-11-15.md`): deleting another project's
  evaluator answers 404 instead of naming that project's online rules; deleting another org's
  connection answers 404 while it is in use; a control-plane org creation clears the password,
  sessions and tokens of an unverified account it makes owner; a disabled owner no longer counts as
  the org's last owner; an invitation to an org that is not active cannot be accepted; a trusted
  proxy seen as an IPv4-mapped IPv6 address is trusted; a config file with a TOML syntax error no
  longer prints the offending line.
- Online evaluation: an exhausted Anthropic credit balance or Gemini daily quota now pauses the rule at once, like OpenAI's; the egress guard also denies `0.0.0.0/8`, `240.0.0.0/4`, `::/96`, `64:ff9b:1::/48`, `2002::/16` and `fec0::/10`; a judge or webhook header can no longer override `content-type` or the auth header.
- The default query memory pool now honours a systemd `MemoryMax=` (or any cgroup limit of the process), not only a container's limit.
- A local object write that fails, for example on a full disk, no longer leaves its temporary file until the next start.
- A query that would spill past the free disk above `storage.min_free_bytes` now fails as too large (422) instead of filling the disk.
- The S3 read cache no longer writes blocks on a stale free-disk measurement, so it cannot take the disk below `storage.min_free_bytes`.
- With S3, a multipart upload that fails is now aborted, so its parts no longer stay on the bucket (track R).
- With S3, a file that passes verification has its read-cache blocks dropped, so a bad cached block no longer keeps failing queries (track R).
- Storage: a local object deleted by garbage collection, or a directory created just before a crash, could reappear or disappear after a power loss.
- Storage: in interval mode (`wal.sync = "<duration>"`), a power loss could leave a torn WAL segment that was no longer the last one, and the next start refused it as corrupt. A segment is now fsynced before a newer one is created.
- Storage: a WAL group whose fsync failed is no longer truncated with a second fsync, and the segment a failed group discards stays removed after a power loss.
- Storage: a read-cache size file torn by a power loss could make S3-mode queries read short ranges. The size file now carries a CRC-32C; cache directories written in the old format are dropped at open and fetched again.
- The span list no longer shows a re-sent span twice, and its pages no longer depend on the page size. A re-send that changed its `start_time` is still listed twice until compaction merges them.
- Storage GC no longer deletes a data file that a query started before a deletion pass still reads (the deletion's raise of a clean file hid the query's reference).
- The trace list no longer hides a trace ingested after a deletion of the same trace id while another deletion of the project is pending.
- The traces page shows onboarding only for a project with no trace at all (an all-time one-row probe); a project whose traces are older than the selected range sees its empty range instead.
- The live traces list refetches its first page at most once at a time: a busy stream no longer cancels the refetch it started, and the traces that arrived meanwhile are fetched by one trailing refetch.
- The live traces list and the trace view stop their stream on a 4xx answer instead of retrying forever, a 401 on a stream goes to `/login`, and the live list opens no stream while it shows an invalid filter.
- The org's project list says "Forever" for a project that keeps data forever, instead of "Default".
- The `/cli-auth` page hands the new CLI token over in a form POST instead of a URL, so it stays out of the browser history; the token expires after 90 days, and the page warns about links sent by someone else and shows the CLI's host as reported.
- OTLP/gRPC checks the credential, the project and the units cap before decoding a message, and
  reserves the in-flight budget for the largest message before the body is read. The gRPC listener
  allows 100 concurrent streams and 32 in-flight requests per connection, and cuts a request
  after 30 s, so a stalled stream releases its reservation.
- Score alerts count scores created after their trace ended (judge, feedback and annotation scores),
  for traces that started up to 7 days before the window (D-K-17).
- Usage counts are no longer lost when shutdown cuts a usage flush short.
- Identical dashboard metric requests that miss the cache together run the query once, and a full
  cache drops its oldest answers instead of all of them.
- `cetra login` exits at once when the sign-in is cancelled in the browser, instead of waiting five
  minutes for a callback that never comes.
- A password reset (forgot-password link or admin reset link) now revokes the user's personal
  access tokens as well as their web sessions, as a password change already did.
- A categorical webhook evaluator must list its categories; one saved without them is refused with
  400 (`output_score.categories`) instead of failing every run with `unknown label`.
- A webhook evaluator answering 402 now pauses its rule, as D160 says for every other evaluator
  (it was permanent, so each run failed until the error breaker tripped).
- SQL console answers and exports are bounded in bytes. A JSON answer stops at the last row that
  fits 32 MiB of encoded output and is marked `truncated` like a row-limit cut; only a first row
  over the cap, or an encoding that does not fit the query memory pool, is `422 query_too_large`
  (D98). Aliasing a heavy column or cross-joining heavy rows could previously multiply the output
  past any memory limit.
- CSV exports prefix a string cell or header name that starts with `=`, `+`, `-`, `@`, tab or CR
  with `'`, so a spreadsheet shows it as text instead of running it (D-A2-5). NDJSON and Parquet
  are unchanged.
- The SQL console lists every saved query you can see, not only the first 1,000, and opens a
  `?view=` link to a query past the listed pages.
- Deleted spans and the spans of a dropped project leave the WAL by the end of the next flush cycle,
  not when the newest WAL segment fills (64 MiB): each committed flush starts a new segment when
  the open one holds records, and deletes the segments it has flushed (D-A1-14).
- `cetra query` finds `{name:Type}` placeholders after a nested `/* … /* … */ … */` comment, as the
  server does, instead of reading the rest of the statement as a string.
- An export whose client stops reading for 30 s releases its query, and its memory admission, instead
  of holding them until the connection closes.
- A media blob whose declared MIME type is longer than 255 bytes is sniffed instead of failing the whole OTLP request. Before, the request got a 400 and the exporter dropped every span in it.
- Promoted string columns (`session_id`, `user_id`, `environment`, `release`, `request_model`, `response_model`, `provider`, `operation`, `request_params`, service and scope names, `tags`, metadata values, event names and link attributes) are cut to `max_attribute_bytes` like other strings, and listed in `truncated`. Before, a multi-megabyte `session.id` was stored whole.
- The built-in price table prices Gemini (Google AI and Vertex AI), Azure OpenAI, Azure AI Inference, xAI and Mistral spans. It keeps one row per model and provider, and the Google GenAI and OpenInference provider names are recognised (D-B1-11).
- The Python SDK parses a score `Retry-After` header as the spec and the TypeScript SDK do: ASCII digits, surrounding whitespace ignored. `+5`, `1_0` and fullwidth digits now use the default backoff, and a value too large to convert is capped at 30 s instead of failing.

### Changed

- A dropped organization's daily usage is kept 40 days after the drop, for billing, then deleted.
- `/readyz` also checks the object store, free disk and storage failures; a full disk or an
  unreachable bucket now makes the server not ready. `/healthz` is the liveness check and fails on
  the storage failures a restart clears.

### Upgrade notes

- Cloud cells must set `entitlements.control_plane = true`; without it, `/api/internal/v1/*`
  answers 404 and org and control-plane tokens are refused (D-P-50).

### Removed

- `GET /metrics`: every chart now reads `POST /dashboards/query` (D-C-16).
