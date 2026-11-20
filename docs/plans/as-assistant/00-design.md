# AS · In-app assistant — design

Track AS adds a chat assistant to the web UI (D193). It answers questions about the project's traces, sessions, metrics, evaluations, dashboards and settings, and helps build filters, SQL, dashboards, evaluators and online rules. It runs in the server as a tool-calling loop over the org's own LLM connection. Its tools read through the same code paths, permissions and data window as the API. It never writes: what it builds comes back as a draft that the user opens in the normal editor and saves there.

This document holds the goals, the architecture, the decisions and the milestones. Each milestone gets its own plan, written from the code that exists when it starts (`~/.claude/CLAUDE.md`, `AGENTS.md`).

**Read first:**
- `AGENTS.md`;
- `docs/mvp-decisions.md`: D21, D159, D160, D166, D173–D179, D181, D187, D193, track H (D-H-9–D-H-11), track I (D-I-4–D-I-6, D-I-24, D-I-30, D-I-31), track P (D-P-21), D-C-16, D-N-25;
- track V's design, `docs/plans/v-prompts/00-design.md` (PR #181): its streaming provider layer (D-V-19), SSE transport (D-V-20), reserved environment (D-V-22), units (D-V-23) and limits (D-V-24), which this track builds on;
- specs 04, 05, 06, 08, 09, 11 and 12;
- `crates/server/src/mcp/` (the seven read tools), `crates/eval/src/judge/` (the provider adapters);
- the competitor survey below, and its full notes in the session notes folder (`as-assistant-competitors.md`).

## Goals

- A chat panel in every project page. The user asks in plain language: "why did errors spike yesterday?", "which users cost the most this week?", "what does this trace do wrong?", "is my faithfulness judge agreeing with the human scores?".
- Answers grounded in the user's data, through tool calls the user can inspect: the SQL it ran, the traces it opened.
- Help to build things: a list filter, a SQL query, a dashboard or a widget, an evaluator, an online rule. Each comes back as a validated draft with an Apply button.
- Awareness of the current page: the open trace or span, the list's filter and range, the SQL in the console, the dashboard on screen.
- The same product on the cloud and self-hosted, on every plan, with the org's own provider paying.
- Its own calls recorded in cetra, with cost, like judge traces and playground runs.

## Non-goals

- **Writes by the assistant.** No tool creates, changes or deletes anything (D-AS-6). Approval-gated write tools are left for after the MVP.
- **Code execution or a sandbox.** No shell, no Python, no per-conversation pod (LangWatch, Langfuse and MLflow need one; a single binary should not).
- **Fetching the web**, including the documentation site. The model knows the product from a built-in guide in its system prompt and from `get_schema` (D-AS-13).
- **A cetra-hosted model.** The org's LLM connection pays, on the cloud too (D-AS-2).
- **New MCP tools.** The tool code is shared with H's MCP server, but `/mcp` keeps its seven tools (D-AS-5).
- **Org-wide or cross-project questions.** A conversation belongs to one project.
- **Resuming a turn after the browser disconnects.** Closing the tab cancels the turn in progress (D-AS-9).
- **Shared conversations** and admin access to other people's conversations.
- **A user-editable system prompt** and per-project assistant settings.

## Architecture

### Parts and crates

The enforced crate graph (`AGENTS.md` § Layout) does not change. No crate gains a workspace dependency.

| Crate | What AS adds |
|---|---|
| `cetra-core` | The reserved environment `cetra-assistant`, added to the "is this environment reserved" test that V's M6 introduces (D-V-22). A shared contract: the coordinating session makes the change, or grants it in the milestone's brief. |
| `cetra-eval` | Nothing new in the provider layer: AS calls V's streaming chat layer (D-V-19) with tools. A trace builder for assistant turns beside V's playground-run builder, on the same OTLP → `normalize` path as judge traces (D161). |
| `cetra-server` | The org assistant setting and its test (D-AS-3). `ProjectPerm::UseAssistant` in `auth/roles.rs`, mirrored in `web/src/lib/roles.ts` (D-AS-4). Shared read functions extracted from the route modules the new read tools mirror (D-AS-5). The `meta.db` conversation tables, their expiry and deletion (D-AS-10). The tool registry: the MCP read functions, new read tools and the draft tools with their validators (D-AS-5, D-AS-6). The turn loop and its SSE route (D-AS-8, D-AS-9). Limits (D-AS-15). The internal commit of turn traces (D-AS-11). |
| `tools/capture` | The mock provider scripts tool calls in its streamed answers, so the turn loop has an end-to-end test without a real provider. |
| `web/` | The assistant panel, its history list, context chips, tool-call steps, draft cards and their Apply handlers on each target page; the org setting page. |
| `website/` | A user page for the assistant, and the setting in the org settings page. |

```
 web panel ── POST /projects/{p}/assistant/conversations/{c}/turns (session) ──► server
     ▲  text/event-stream: start, text, tool_call, tool_result, draft, done, error      │
     │                                                                                   ▼
     │                                     turn loop (≤ 16 model calls, ≤ 10 min)
     │                                        │ load history (meta.db), page context
     │                                        │ V's chat layer ─► egress (D166) ─► org's provider
     │                                        │ tool calls ─► tool registry
     │                                        │                 ├ read tools: MCP functions + new reads
     │                                        │                 │   (user's permissions, data window, limits)
     │                                        │                 └ draft tools: validate, return a draft
     │                                        │ store messages (meta.db: assistant_*)
     │                                        └ turn trace, metadata only (OTLP → normalize, env cetra-assistant) ─► storage
     │
     └── "Apply" on a draft ─► the target page opens prefilled ─► user saves through its own route
```

### Tables (`meta.db`)

| Table | Holds |
|---|---|
| `org_assistant_settings` (M3) | one row per org: `enabled`, `connection_id` (FK to `llm_connections`, `ON DELETE SET NULL`), `model`, `updated_by`, `updated_at` |
| `assistant_conversations` (M1) | `id` (UUIDv7), `project_id`, `user_id`, `title`, `created_at`, `last_active_at`; cascades with the project, the org and the user |
| `assistant_messages` (M1) | `conversation_id`, `seq`, `role` (`user`, `assistant`, `tool`), `content` (canonical message parts as JSON: text, tool calls, tool results, drafts), `context` (the page context sent with a user message), `status` (`complete`, `cancelled`, `failed`), `feedback` (null, `up` or `down`, on assistant messages), `turn_trace_id`, `created_at`; cascades with the conversation |

Exact columns, indexes and caps are M1's plan. They follow spec 04's conventions (time in ns, UUIDv7 ids).

### API (`/api/v1/…`)

| Route | What |
|---|---|
| `GET/PUT /orgs/{org}/assistant` | the org setting (`enabled`, `connection_id`, `model`); reading needs org `View` (members already list connection ids, D-I-4), writing needs `OrgPerm::ManageConnections` (org admin, D-I-24) |
| `POST /orgs/{org}/assistant/test` | one tool-calling call against the chosen connection and model; answers `{ok, model, latency_ms, error?}` like D-I-5 |
| `GET /projects/{p}/assistant` (`ProjectPerm::View`, session only, so a viewer gets `forbidden` instead of a `403`) | what the panel needs to show: whether the assistant is usable by the caller, the connection's name and `adapter` for the notice, and the reason when it is not (`disabled`, `no_connection`, `restore_mode`, `forbidden`) |
| `GET/POST /projects/{p}/assistant/conversations` | the caller's conversations, newest first; create one |
| `GET/PATCH/DELETE /projects/{p}/assistant/conversations/{id}` | read with messages, rename, delete; only the author |
| `POST /projects/{p}/assistant/conversations/{id}/turns` | send a user message with its page context; answers `text/event-stream` (D-AS-9) |
| `PUT /projects/{p}/assistant/conversations/{id}/messages/{seq}/feedback` | thumbs up, down or cleared on an assistant message (D-AS-11) |

Every other `/projects/{p}/assistant…` route is `session_only: true` with `ProjectPerm::UseAssistant` (D-AS-4).

### Tools

Every tool runs with the caller's `AuthUser` and the conversation's project injected, through `user_project(state, user, project, perm)` with the same `ProjectPerm` as the route that serves the same data (D-AS-5). The model never sees a `project` argument. Since `UseAssistant` needs member, every read below passes for a caller who can chat; the per-tool check still runs, so a later change to a route's permission reaches the tool.

| Tool | Kind | Source | Permission |
|---|---|---|---|
| `get_schema`, `query_sql`, `search_traces`, `get_trace`, `get_span`, `list_saved_views` | read | H's MCP functions, unchanged, with their limits (D-H-9, D-H-10) | as in MCP: `View`, `Query` for `query_sql` |
| `query_metrics` | read | the dashboard builder query (`POST /dashboards/query`, spec 05 § Dashboard queries) | `Query` |
| `list_sessions`, `get_session` | read | the session list and detail reads (`routes/sessions.rs`) | `View` |
| `list_scores` | read | the score reads and the score definitions (`routes/scores.rs`, `routes/score_definitions.rs`) | `View` |
| `list_evaluators`, `get_evaluator`, `list_online_rules`, `get_online_rule` | read | I's reads with the stats summaries (D-I-48, D-I-50) (`routes/evaluators.rs`, `routes/online_rules.rs`); webhook values stay sealed | `View` |
| `list_dashboards`, `get_dashboard` | read | the dashboard reads (D-C-16) (`routes/dashboards.rs`) | `View` |
| `list_alert_rules` | read | K's reads (`routes/alert_rules.rs`); channel secrets stay sealed | `View` |
| `get_project_settings` | read | what `GET /projects/{p}` answers (`routes/projects.rs`) | `View` |
| `draft_filter` | draft | a filter (DSL text), search and range for the traces, spans, sessions or users list | none beyond `UseAssistant` |
| `draft_sql` | draft | a SQL query with params and range for the console | none |
| `draft_dashboard` | draft | a new dashboard definition, or an edited definition of an existing one (with its `version`) | `View` when it edits an existing dashboard |
| `draft_evaluator` | draft | an evaluator (judge or check; never a webhook, whose secret the model must not handle) | none |
| `draft_online_rule` | draft | an online rule over an existing evaluator | `View` (it reads the evaluator) |

The MCP functions already have the in-process shape `(state, user, arguments)`. The other route handlers take axum extractors (`State`, `Extension<ProjectAccess>`), so M2 extracts, from each route module named above, one function that both the route and the tool call; nothing in those modules is duplicated. The exact argument schemas and result shapes are M2's plan.

Each read keeps its route's exclusions: `search_traces` and the session reads apply the default exclusions, so `cetra-assistant` (D-AS-11) stays out unless the model's filter names `environment` (D-V-22), and `query_sql` hides nothing, as in MCP. This is accepted: turn traces hold no message text, arguments or results (D-AS-11), so reading them shows only that a turn happened, by whom, its cost and the tools it used. The system prompt tells the model that rows of that environment are the assistant's own records.

## Decisions

"(user)" marks the user's choices of 2026-11-20. The others are this design's proposals, under review with this document, and are recorded in `docs/mvp-decisions.md` § Track AS.

| # | Decision | Rejected |
|---|---|---|
| D-AS-1 | **Plan form: a design doc and four milestones** (§ Milestones). M1 conversations, M2 tools, M3 turns and the org setting, M4 UI and closeout. M1 and M2 can run in parallel. | One plan: the turn loop depends on V's M5 and M6, the UI on the loop, so one plan would idle while V lands. |
| D-AS-2 | **The org's LLM connection pays (user).** The assistant calls one of the org's LLM connections (D-I-4) through V's streaming chat layer (D-V-19). The same holds on the cloud and self-hosted. Without a usable connection the panel explains how to set one up. | A cetra-hosted model on the cloud: a cetra provider key on every cell, a per-org quota, a cost line in Q and a second privacy story. Self-hosted would still need BYO. Langfuse and Opik do it and pay for it in operator keys and privacy questions. |
| D-AS-3 | **Off until an org admin turns it on (user).** One org setting: `enabled`, `connection_id`, `model`. Org admins write it (`OrgPerm::ManageConnections`, D-I-24). The form says, next to the switch, that the content the assistant reads (prompts, completions, attributes) is sent to that connection's provider. Saving with `enabled` runs the setting test first: one streamed call that must return a forced tool call, because the loop needs tool calling. A model without tool calling is refused with the provider's error class. A PUT with `enabled` and no connection is a `400`. Deleting the connection sets `connection_id` to null, and a null connection means off whatever `enabled` says; the connection delete route, which refuses a connection that evaluators use (D-I-19), does not refuse one the assistant uses. Assistant calls and the setting test never change the connection's `status` and never pause rules (D-I-5, D-I-14 stay about evaluation): a 401 in a chat shows in the panel, and an admin fixes the connection. A connection marked `failing` still serves the assistant. The setting and its test ship together in M3, so the setting can never be enabled without the test. The demo org (D187) has no special case: its implicit viewers never see the panel, and the team decides whether to enable it there. | On by default once a connection exists: payload content would start flowing to a provider without anyone choosing it. Per-project opt-in: one more switch for a choice that follows the connection, which is org-wide. |
| D-AS-4 | **Members and above, through a session (user).** A new `ProjectPerm::UseAssistant`: minimum role `member`, key scope none; every assistant route is `session_only: true`, so no PAT and no API key reach it. V's `RunPlayground` (D-V-26) is added to the same enum and table; whichever lands second rebases on the first. Viewers, including the demo org's implicit viewers (D187), do not see the panel. A chat spends the org's provider money, as a playground run does (D-V-26). Coding agents use `/mcp` with a PAT instead. Reading and deleting one's own conversations needs the same permission; losing it hides them until it comes back or they expire. | Viewers chatting: they would spend money that admins pay for. Read-write PATs: no client needs the turn route outside the UI. |
| D-AS-5 | **Read tools share MCP's code (user).** The six project tools of H's MCP server are called in process, as the functions `crate::mcp::*` already expose (`(state, user, arguments)`), with `project` set by the server. `list_projects` is left out: a conversation has one project. New read tools (§ Tools) call one function shared with their HTTP route, extracted in M2, with the route's `ProjectPerm` (§ Tools lists each), so the assistant can never read more than its user. Every result goes through MCP's output rules: compact JSON, strings cut to 200 characters, a `max_chars` cap (default 20,000) and `notes` naming every cut (D-H-11). `/mcp` itself keeps its seven tools in this track. | A separate tool implementation: two copies of the access checks. Growing `/mcp` at the same time: the user kept MCP as it is. A single SQL tool only: models need ids and structured trace trees, which `get_trace` already gives. |
| D-AS-6 | **Drafts, not writes (user).** No tool writes. A draft tool validates its arguments with the same validators as the route that would save it: the storage filter parser for a filter (with `position` on error), the SQL sandbox's parser for SQL, the dashboard definition rules, the evaluator and rule validation of D-I-30 (except checks that need the saved evaluator's sealed values). An invalid draft is a tool error the model sees and can fix. A valid draft is stored as a draft part of the assistant message and streamed as a `draft` event. The panel shows it as a card: "Apply filter", "Open in SQL console", "Open in dashboard editor", "Open evaluator editor", "Open rule editor". Apply opens the target page prefilled and unsaved; the user saves it through that page's own route and permissions, so a member's draft of a rule still needs `ManageEvaluation` to save. An evaluator draft never holds a connection secret: it names an existing connection id. Because a draft's text can come from injected trace content, the evaluator and rule cards show the full judge template, mapping and filter before Apply, not a summary. | Write tools with an approval card (Phoenix, Langfuse): a second permission path to test, and prompt injection from trace payloads becomes a write risk. Read-only answers with copy-paste: loses the "build it for me" goal. |
| D-AS-7 | **Trace content is untrusted input.** The model reads prompts and completions that anyone who can send spans controls. So:<ul><li>no tool writes (D-AS-6) or reaches the network;</li><li>the panel renders Markdown without raw HTML and without images, so an answer cannot make the browser fetch a URL;</li><li>links to app paths become in-app links, and any other link shows its full URL and opens only on click, in a new tab with `noopener noreferrer`;</li><li>the system prompt tells the model that tool results are data, never instructions;</li><li>drafts are validated server-side and applied only by the user's click.</li></ul> | Trusting the model to ignore injected instructions: competitors' render paths fetch remote images. |
| D-AS-8 | **The turn loop runs in the server.** A turn is one user message and everything until the model's final answer. The loop: <ol><li>load the conversation and build the model input: the built-in system prompt, the page context, the history (D-AS-15's budget), the tool definitions;</li><li>call the model through V's chat layer, streaming text out as it comes;</li><li>run each complete tool call (several calls of one step run one after the other, in order), stream its result summary, append the result;</li><li>repeat until the model answers without tool calls, or a limit stops it.</li></ol>Each model call takes the connection's concurrency slot and rate token like a playground run (D-V-24, D-I-6) and gives them back when the call ends; nothing is held across tool calls or between steps. A call that cannot get them within 10 s ends the turn with `error` `{class: "retryable", message: "connection busy"}`, and the partial answer is stored as `failed`; the user sends again. | A separate agent service or sidecar: one binary (`AGENTS.md`). The loop in the browser: the browser would need the tools' results and a key, or one round-trip per step through the server; the keys are sealed server-side (D-I-1). |
| D-AS-9 | **Transport: SSE answering a POST, like the playground (D-V-20).** `POST …/turns` with `{message, context}` validates, stores the user message and answers `text/event-stream`: <ul><li>`start` `{turn_id, trace_id}`;</li><li>`text` `{delta}`;</li><li>`tool_call` `{id, name, arguments}`;</li><li>`tool_result` `{id, ok, summary}` (row counts, ids, error code; the full result is in the stored message);</li><li>`draft` `{id, kind, draft}`;</li><li>`done` `{finish_reason, usage, cost_usd, steps}`;</li><li>`error` `{class, message}` with D160's classes and no payload text.</li></ul>A ping every 15 s and D-G-34's 30 s no-read rule. A turn is not a live stream: it does not count toward D-G-34's per-user stream cap, and D-AS-15's per-user limits and its 10-minute cap bound it instead. Aborting the fetch cancels the turn: the provider call is dropped, the partial answer is stored with status `cancelled`. One turn at a time per conversation (`409`). | WebSockets: a second protocol for a request-response flow. A durable run with an event log and a cursor tail (Langfuse): survives a closed tab, at the cost of a job queue and an event table; the panel lives in the app shell, so navigation does not close it. |
| D-AS-10 | **Conversations in `meta.db`, private, 30 days (user).** A conversation belongs to its author in one project; nobody else, admins included, can list or read it. It is deleted 30 days after its last activity, when its author deletes it, and with its project, its org or its author's account. A user keeps at most 100 conversations per project (the oldest is deleted to make room) of at most 200 messages each. The title is the first user message cut to 80 characters, and can be renamed. Not hidden by the data window (D175): it is the user's own text. | Browser storage only (MLflow): lost across browsers, and the traced calls hold the content anyway. Kept until deleted: unbounded copies of payload excerpts. |
| D-AS-11 | **Turns are traced into the project, in the reserved environment `cetra-assistant`, as metadata (user).** One turn is one trace: an `agent` root span, one `llm` span per model call (model, adapter, usage, cost from the project's price table, `ttft`, finish reason) and one `tool` span per tool call (tool name, duration, result size and row count, error code). No span holds message text, tool arguments or tool results; the content lives only in the private conversation (D-AS-10, D-AS-12). Metadata holds `cetra.assistant.user` (the user id), `cetra.assistant.conversation_id` and the system prompt's version. It is built as OTLP and normalized like judge traces (D161), then committed in process. `cetra-assistant` is reserved wherever `cetra-eval` and `cetra-playground` are (D-V-22): the SDK refuses it, the OTLP handler rewrites it, and default and strict exclusions leave it out. Its spans are not units (D-V-23): no units-cap check, no rule budget. The traces follow the project's retention and are dropped with the project or org; a user's account deletion leaves them, since they hold no content. Thumbs up or down set `feedback` on the stored assistant message; they are not scores, so they spend no unit and need no score definition. | Content in the trace: every project reader would read every member's private questions, beyond the conversation's expiry and the author's deletion. No trace, counters only: no per-turn cost or latency record. A hidden internal project: a project the user cannot see. Thumbs as a human score: a unit spent per click, a `402` at the Free cap, and a score definition to manage. |
| D-AS-12 | **Copies of payload content.** Tool results are the assistant's copies of trace data. <ul><li>The turn trace keeps none (D-AS-11).</li><li>The stored history keeps full tool results for 24 hours after their turn (a constant, `STUB_AFTER`, chosen to match D181's 24 hours), so a follow-up question can use them; then a periodic job replaces each by a stub naming the tool, the size and the row count, and the model calls the tool again if it needs the data. A result kept in that window is not re-checked against a later deletion, downgrade or data-window change.</li><li>The model's answers, the user's messages and the page context (which can hold filter and SQL text the user typed) stay until the conversation is deleted, 30 days at most.</li></ul>A deletion by user id, trace or filter (D24) does not scan conversations. Stubbing and deletion rewrite `meta.db` rows; the old bytes stay in free pages, the WAL and backups until SQLite reuses them, which D181's wording already covers. The website's privacy page says that answers and messages may quote trace data for up to 30 days. | Full results for the conversation's 30 days: copies no deletion reaches for a month. No stored results: every follow-up re-runs every query. |
| D-AS-13 | **A built-in system prompt.** It ships in the binary with a version string recorded on every turn trace. It holds the role, the untrusted-data rule (D-AS-7), the current UTC time, the project's name, a short product guide (the filter DSL grammar, the views and their key columns, the dashboard definition shape, the evaluator kinds and mapping syntax, the units and plans in one line each) and the answer style (cite trace ids as app links, show the SQL it ran). The full schema comes from `get_schema` on demand. Not editable by users in the MVP. | Retrieval over the documentation site: an index to build and ship, or a network fetch (D-AS-7). A user-editable prompt: a support burden before anyone asks for it. |
| D-AS-14 | **Page context is a small typed object.** The panel sends, with each user message, the page kind and its ids and state: a trace or span (ids, `start_time`), a session or user id, a list (view, filter, search, range), the SQL console (query text, params, range), a dashboard (id), an evaluator, a rule, a settings page. At most 8 KiB, as JSON in the system message. It is sent to the provider and stored with the message for the conversation's life (D-AS-12). The panel shows it as chips the user can remove before sending. The server checks only its shape and size: every read still goes through the tools' permission checks. | Sending the page's rendered data: large, and duplicated by the tools. The URL alone: loses filters kept outside the URL (D-H-13). |
| D-AS-15 | **Limits.** <ul><li>Per user: 20 turns a minute and 2 at once; per org: 8 at once (`429`).</li><li>Per turn: at most 16 model calls and 32 tool calls, 10 minutes, and each call under V's per-call rules (300 s, 60 s idle, D-V-24). `max_tokens` 4,096 per call. A limit ends the turn with a final short answer saying which limit stopped it.</li><li>A user message is at most 16 KiB.</li><li>These limits are the assistant's own: turns do not count against the playground's per-user and per-org run limits (D-V-24), and playground runs do not count against them. Both share each connection's concurrency and rate (D-I-6).</li><li>The history sent to the model is at most 100,000 estimated tokens (characters / 4): the oldest tool results are stubbed first, then the oldest turns are left out; the system prompt, the context and the latest turn always go.</li><li>Tool results use MCP's limits (D-H-10: SQL at most 1,000 rows and 30 s; `max_chars` 20,000).</li></ul> | MCP's per-PAT quota (60 a minute): one turn can make 32 tool calls. No history budget: long conversations would fail on the model's context window. |
| D-AS-16 | **Data window, exclusions and restore mode.** Every read goes through the shared read functions and `entitlements::query_options` with `Visibility::window` (D-P-21, D-P-44), so the assistant sees what its user sees. Restore mode refuses turns and the setting test (`503`), as it closes egress (D-N-25); `GET /projects/{p}/assistant` reports `restore_mode`, and conversations can still be read, renamed and deleted. | |
| D-AS-17 | **MIT core, on every plan (user).** Nothing under `ee/`, no new feature name in D-P-12, no message cap: the org pays its provider. | A paid-only feature or a free message cap: competitors that gate it fragment the product, and BYO already bounds cetra's cost to nothing. |
| D-AS-18 | **The panel.** A right-hand side panel in the project shell, opened from the top bar or with `Ctrl/Cmd + I`, resizable, open across navigation within the project. It shows: the history list and "New chat"; the transcript with Markdown answers (D-AS-7); each tool call as a collapsed step (name, arguments, SQL shown as code, result summary, a link to the trace or the console); draft cards; a Stop button; thumbs on answers; context chips above the composer; and one line under it naming the connection and its provider ("Answers use *OpenAI prod*; the data it reads is sent to that provider"), with the connection's `adapter`. An off assistant shows why (not enabled, with a link for admins; no connection; restore mode). | A full page: loses the page context the user is looking at. |

## Open questions for the user

- **Q1. Stubbing tool results after 24 hours (D-AS-12).** It keeps the assistant's raw copies of trace data within D181's 24 hours, but a follow-up question the next day re-runs its queries, and the model's answers and the user's messages may still quote data for 30 days. Is that the right trade, or should results be kept for the conversation's 30 days with the privacy page saying so?
- **Q2. Turn traces without content (D-AS-11).** You chose to trace the assistant's calls. The design keeps the traces to metadata (models, cost, latency, tool names, sizes), because content in a project trace would let every project reader read every member's private questions. Debugging a bad answer then happens from the author's conversation. Is metadata-only right, or should traces carry the content and conversations stop being private?
- **Q3. The MVP scope summary.** `docs/mvp-decisions.md` § MVP scope at a glance has a line per feature. Should the coordinator add an "Assistant" line, as V's PR adds "Prompts"?

## Milestones

Each milestone gets its own plan, written when it starts. Each one updates its spec sections (04, 05, 08, 09, 11, 12), its documentation pages and `CHANGELOG.md` in the same PR.

| Milestone | Delivers (what works when it is done) | Depends on | Leaves out |
|---|---|---|---|
| **M1** Conversations | The conversation tables exist; conversations and their messages can be created, listed, read, renamed and deleted by their author only, through sessions, with `ProjectPerm::UseAssistant` (added to `auth/roles.rs`; its web mirror comes with M4, its first caller). The 30-day expiry and the 24-hour tool-result stubbing run as a periodic job; caps hold; deletion cascades with project, org and user. OpenAPI and `fixtures/api` cover every route. | Nothing in AS (tracks D, G, I, P are done) | The org setting, turns, tools, UI |
| **M2** Tools | The tool registry: the six MCP functions with the project injected, the new read tools over functions extracted from `routes/sessions.rs`, `scores.rs`, `score_definitions.rs`, `evaluators.rs`, `online_rules.rs`, `dashboards.rs`, `alert_rules.rs` and `projects.rs` (each route then calls the same function), the five draft tools and their validators, each with its JSON schema and description. Each tool is tested against the example dataset for its result, its permission check (a project the user cannot read) and its limits. MCP's and the routes' own tests pass unchanged. | Nothing in AS; runs alongside M1. Shared files: `crates/server/src/lib.rs`'s module list only (M1 owns `auth/roles.rs`) | Calling a model |
| **M3** Turns and setting | The org setting and its test, with the connection delete route allowing an assistant connection; `GET /projects/{p}/assistant`. The turn route streams a whole turn: history and budget, page context, the loop over V's chat layer, tool execution, drafts, cancellation, stored messages, limits, restore mode; feedback on messages. Each turn is traced in `cetra-assistant` as metadata only, priced, not counted as units. The capture mock provider scripts tool calls, and a server test runs a full turn against it. | M1, M2 merged; V's M5 (chat layer) and M6 (the reserved-environment test in `cetra-core` and the units rule) merged. Adding `cetra-assistant` to V's list is a one-line `cetra-core` change, made by the coordinator or granted in M3's brief | UI |
| **M4** Panel and closeout | The panel, the history, context chips per page kind, tool steps, draft cards and the prefilled Apply on the list pages, the SQL console, the dashboard editor and the evaluator and rule editors; the org setting page with the test and the notice. The documentation page and its screenshots. An end-to-end test asks a question over the example data with the mock provider, gets an answer and applies a filter draft. Roadmap row done. | M3 merged | Q1–Q3 unless the user changes them |

M1 and M2 run in parallel. M3 waits for both and for V's M5 and M6. M1 creates no turn trace, so it does not need V's reserved environments. M4's page work can start on MSW mocks of the M3 stream once M3's event shapes are in the plan.

## Competitors

From the code of Phoenix, LangWatch, Langfuse and MLflow, and the open parts of Opik and Laminar (full notes: `as-assistant-competitors.md` in the session notes folder).

| | Phoenix PXI | LangWatch Langy | Opik Ollie | MLflow | Laminar | Langfuse |
|---|---|---|---|---|---|---|
| Tools | GraphQL through a virtual shell; REST as in-memory MCP; typed UI operations run in the browser | shell, files, skills and the `langwatch` CLI | closed; read-only SQL; artifact proposals | Bash, Read, Write, Edit, skills | OSS: MCP SQL and trace context | ~70 MCP tools, docs, a sandbox |
| Writes | yes, with approval (default) | yes, a clamped key; deletes need a click | proposals the host applies | permission prompts | read-only | per-tool approval after an RBAC filter |
| Where, whose key | Phoenix server, BYO provider | a separate Go agent pod per conversation, BYO keys | Comet-hosted, credits | server subprocess (Claude Code, Codex) or OpenAI-compatible | server, workspace profiles | worker queue, the operator's model |
| Stream | AI SDK stream over SSE | event log and token lane | JSON lines | SSE with resume | — | events persisted, SSE tail |
| Storage | DB, 30 days idle | ClickHouse events | per-user server blob | temp files 24 h and localStorage | server, private | Postgres, retention (EE) |
| Privacy notice | none; an off switch | cloud-only note | — | none | — | org AI toggle |
| Self-traced | opt-in, own project | always, in the user's project | — | no | — | to the vendor's project |
| Gating | on by default | rollout-gated | cloud, paid | local default | enterprise | cloud on, self-hosted opt-in |

Taken: one tool surface shared with MCP; permissions by construction (the user's own principal); proposals the page applies (Opik); typed, capped, removable page context; caps on tool output with notes; per-turn traces (Phoenix), kept to metadata so private questions stay private; conversations private per user with a retention (Phoenix). Avoided: sandboxes and per-conversation pods, vendor-paid models, browser-only transcripts, an agent that reads more than its user (Laminar documents the gap), remote images in answers, and payloads sent to a provider without an admin choosing it.
