Status: implemented on 2026-11-20 (M1–M5b), with a Sonnet review and fixes per milestone.

# CA · Coding-agent tracing — design

Track CA makes coding agents first-class sources in cetra (D195). Users point Claude Code, OpenAI Codex CLI, Gemini CLI or GitHub Copilot at cetra and see:
- each session as a list of traces: one trace per prompt, with the model calls and tool calls under it;
- tokens and cost;
- the person, the agent, the repository and the branch of each trace;
- a dashboard of cost per person, per repository, per branch, per session, per agent and per model.

**Read first:**
- `AGENTS.md`.
- `docs/research/coding-agents.md`: what each agent emits, what the competitors do, and what our normalizer does with it today.
- `docs/mvp-decisions.md`: D4, D17, D-G-40, D-B1-10, D110, D114, D-C-16, D-C-21, D-P-70, and Track CA.
- Specs:
  - 01 (span, trace, metadata);
  - 02 (conventions, precedence tables, rules 2, 3 and 18, cost, testing);
  - 05 § Dashboard queries;
  - 08 (keys and scopes);
  - 09 (route access);
  - 10 (`capture_content`);
  - 11 § Dashboard;
  - 12.
- `fixtures/otlp/README.md` and `tools/capture`.

This document holds the decisions, the contracts and the milestones. Each milestone gets its own plan, written just before it starts, from the code that exists then. **Haiku implements the plans one sub-phase at a time** and **Sonnet reviews** them. When a plan and the code disagree, the implementer stops and reports.

## Goals

1. **Normalize the agents' native spans.**
   - The spans of Claude Code, Codex CLI, Gemini CLI and Copilot are normalized like any other instrumentation.
   - The kinds are right: the turn is `agent`, model calls are `llm`, tools are `tool`.
   - Tokens and cost are filled, with no double counting.
   - Tool spans carry the tool's name.
   - The prompt is the turn's input when the agent sends it.
   - `session_id`, `user_id`, and the agent, repository and branch are set.
2. **A small hook adds what the spans lack.** The `cetra` binary gets a hook that sends a session's repository, branch and person to the server. The server stamps them on that session's spans at ingest.
3. **One command connects an agent.** `cetra agents setup <agent>` writes the agent's own settings: the OTLP exporter, our key, the content opt-ins and the hook. `cetra agents remove <agent>` undoes it.
4. **A "Coding agents" dashboard template.** It covers cost per person, repository, branch, session, agent and model, plus tool usage. The traces breakdown gains `session_id` to support it.
5. **Tests and docs.**
   - Captured fixtures of each CLI agent.
   - An integration page per agent on the docs site.
   - An end-to-end test from a recorded agent export to the dashboard query.

## Non-goals (and where they go)

- **OTLP logs and metrics** (user, 2026-11-20). D-G-40 stands: they are still accepted and dropped. A later track on logs and metrics would bring:
  - Claude Code's answer text and its own reported cost (log events);
  - Codex's prompt and tool content (logs only);
  - lines of code, commits, PR counts and active time (metrics);
  - Cursor, which exports no spans at all.
- **Cursor.** It has no spans, and its export is Enterprise-only and server-side. It needs the logs track, or hooks that build spans. Post-MVP.
- **Cost per pull request.** It needs a GitHub integration that maps a branch to its PRs (LangWatch's design). The branch is recorded, so cost per branch works now, and a PR join can be added later without new data.
- **A dedicated coding-agents page** (user: a dashboard template, not a page).
- **Rebuilding traces from transcripts.** Our hook never reads the agents' transcript files, whose formats are internal and unversioned. It also never builds spans.
- **A proxy or gateway** (D4: post-MVP).
- **Agent plugins and marketplaces.** No Claude Code plugin package and no VS Code extension. The setup command writes hooks into the user's settings. A plugin can come later and run the same `cetra agents hook`.
- **Windows hook support.** `cetra` ships for Linux and macOS (track N). On Windows the agents can still export spans, set up by hand from the docs page, but without the hook.
- **Qwen Code, opencode, Cline, Kiro, Windsurf.** Qwen Code joins the MVP only if M1's capture shows spans identical to Gemini CLI's (§ Answered questions 4, D-CA-17).

## How it fits together

```
developer machine                                         cetra
───────────────────────────────────────────────           ─────────────────────────────────────────────
Claude Code / Codex / Gemini CLI / Copilot
  └─ native OTLP span exporter ── POST /v1/traces ──────► OTLP ingest ─► normalize (cetra-otel)
                                  (sk key, ingest scope)        │          agent conventions: kinds,
                                                                │          tokens, names, context
  └─ hooks (SessionStart, prompt submit)                        │
       └─ `cetra agents hook <agent>`                           ▼
            reads stdin, runs `git`                       stamp session context (lookup passed to normalize)
            POST /v1/agent-context ─────────────────────► agent_contexts (meta.db + in-memory map)
                                  (same key)                    │
                                                                ▼
`cetra agents setup <agent>` writes the agent's           commit ─► storage ─► traces, sessions, users,
settings once (exporter, key, opt-ins, hook)                        dashboards (template "Coding agents")
```

Four parts change:
- **`crates/otel`**: agent detection, the agent rules and the `AgentContexts` lookup (M2).
- **`crates/server`**: the context route, the store, and passing the lookup to `normalize` (M3).
- **`crates/cli`**: `cetra agents setup | remove | hook` (M4).
- **`web/`**: the dashboard template, and the `session_id` breakdown in `crates/storage` (M5a).

**`crates/core` does not change.**
- The new fields are `metadata` keys.
- A coding agent is not a new `Convention` variant (that enum is a closed core contract, pinned in snapshots, `openapi.json` and the web types). It is a detection result inside `cetra-otel`: § Agent detection.
- `conventions` keeps listing what the attributes match. For example, a Claude Code model call lists `genai`, because it carries `gen_ai.system`.
- The constant for `cetra.span_name` lives in `cetra-otel`.

`tools/capture` gains the agent scenarios (M1).

## Agent detection and rules (D-CA-3)

`cetra-otel` gets a private `enum CodingAgent { ClaudeCode, Codex, GeminiCli, Copilot }` with the wire names of D-CA-4.

**Inputs.** `normalize` decides it per span from four things:
- the resource `service.name`;
- the scope name;
- the span name;
- the span attributes.

The first rule that matches wins.

| Agent | Detected when (verified by the M1 spike on 2026-11-20, research § 6) |
|---|---|
| Claude Code | the span name starts with `claude_code.` (resource `service.name = claude-code`, scope `com.anthropic.claude_code.tracing`) |
| Codex | the resource `service.name` is `codex` or starts with `codex_` (`codex_exec` for `codex exec`; the interactive name is unverified) |
| Gemini CLI | `gen_ai.agent.name = gemini-cli`, or the resource `service.name` is `gemini-cli` |
| Copilot | the scope name starts with `github.copilot` or `copilot`, the resource `service.name` is `github-copilot` or `copilot-chat`, or any attribute key starts with `github.copilot.` (it still matches when a user renames `OTEL_SERVICE_NAME`) |

**Order inside `normalize`.** The agent rules run after the generic extraction (kind, payloads, usage, context) and before cost.

On a detected span they may:
- override the kind, the name, the payloads, the usage and the context fields of § Spans and § Person;
- set the D-CA-4 keys;
- apply the session context.

Then cost is computed on the result. Usage is read from an agent's own token names only on spans of that agent, which keeps generic names like `input_tokens` from being read anywhere else.

**Shape of the code.** Each agent's rules are one function, with no per-agent type (D102 style). The plan pins where in `normalize.rs` they are called.

## Data contract

### Metadata keys (D-CA-4)

The new data goes into keys of the existing `metadata` map, on spans and therefore on traces (spec 01: the root's metadata wins, other keys are added).

| Key | Value | Set when |
|---|---|---|
| `coding_agent` | `claude-code`, `codex`, `gemini-cli` or `copilot` | the span is detected as that agent (D-CA-3) |
| `repository` | `<host>/<owner>/<name>`, lowercased, without scheme, credentials, port, `.git` or trailing `/` (for example `github.com/cetra-ai/cetra`) | from the session context (D-CA-9), else from the agent's own attributes (§ Normalization) |
| `branch` | the branch name as git prints it (`git rev-parse --abbrev-ref HEAD`); absent on a detached HEAD (git prints `HEAD`) | from the session context, else from the agent's own attributes |

Breakdowns, filters, facets and SQL reach these keys as `metadata['coding_agent']`, `metadata['repository']` and `metadata['branch']`. That works on:
- the traces and spans lists and their facets;
- dashboard widgets on the `traces` and `spans` sources;
- the SQL console.

**The sessions view has no `metadata`** (spec 05 § Logical views), so the sessions list cannot be filtered by repository, branch or agent. Users are derived from traces, so filtering by `user_id` works everywhere. Adding `metadata` to the sessions view is post-MVP.

These keys are written only on spans detected as a coding agent. On such a span, the normalized value replaces a client value under the same key. The keys are not reserved anywhere else, so a user's own `metadata['branch']` on ordinary spans is untouched.

### Person (D-CA-5)

On a coding-agent span, `user_id` is the person, taken from the first source that has a value:
1. the agent's account email on the span: Claude Code `user.email` (present when signed in to a Claude account; the spike, with an API key, had none). Codex and Gemini CLI put no person on their spans;
2. the session context's `user` (D-CA-9);
3. the agent's account id on the span: Claude Code `user.account_uuid`;
4. the generic sources of spec 02 § Context fields. These include Claude Code's anonymous `user.id`, and Copilot's `enduser.pseudo.id`, which is added there.

An email is lowercased and trimmed wherever it comes from. This is an agent-specific override of the generic precedence. Today `user.id` would win, and for Claude Code that is a random install id. The users page then lists people, and "cost per person" is the existing `user_id` breakdown.

**Known limit.** The spans of a session that arrive before its first context post keep a lower-rank id, and they are never rewritten. A person can therefore appear twice for one session: once under an account id, once under the email. The hook posts at `SessionStart`, before the first export, and the docs say to start a new session after setup. So this happens only when a hook fails.

### Spans (D-CA-6)

| Agent | Turn (trace root) | Model call | Tool |
|---|---|---|---|
| Claude Code | `claude_code.interaction` (already `agent` through `gen_ai.operation.name = invoke_agent`); `user_prompt` → `input` (`text`) when not `<REDACTED>` | `claude_code.llm_request` → `llm` (no messages on the span); tokens from `input_tokens` (exclusive of cache, D110), `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`; `ttft_ms` → `ttft_ns` | `claude_code.tool` (already `tool` through `execute_tool`), **named after `gen_ai.tool.name`**; `input` (`json`) is the object of the detail attributes present (`full_command`, `file_path`, `skill_name`, `subagent_type`); `output` (`text`) is the `tool.output` event's `output`, else its `diff`, else its `content`. Its children `claude_code.tool.blocked_on_user` and `claude_code.tool.execution` stay `span` |
| Codex | `session_task.turn` is the trace root and the turn's **only `llm` span**: it carries `model` and the turn's totals (`codex.turn.token_usage.*`, input inclusive of cache, output inclusive of reasoning). Codex puts no model on its per-call usage spans, so per-call spans cannot be priced. Session: `conversation.id`, else the turn's `thread.id`. | — (the turn span) | the dispatch span of each tool (`codex.tool.phase = dispatch`, named after `tool_name`, with `call_id`) |
| Gemini CLI | none: every `llm_call` and every `schedule_tool_calls` is its own trace, so a prompt is several traces of one session | `llm_call` → `llm`, including the model-routing call; `gen_ai.input.messages` / `gen_ai.output.messages` hold **Gemini-native** contents (`parts` of `text`, `functionCall`, `functionResponse`, `thought`; the output is an object or an array), converted to the canonical format | `schedule_tool_calls` → `chain`, `tool_call` → `tool`, named after `gen_ai.tool.name`; `gen_ai.tool.call_id` read as the call id; its input and output messages → `input` / `output` (`json`) |
| Copilot | `invoke_agent` → `agent` (works today) | `chat` → `llm` (works today) | `execute_tool` → `tool` (works today) |

Rules that apply to every agent:
- **Usage only on `llm` spans.** Rule 3 already keeps a turn's own totals (Copilot `invoke_agent`, Codex `codex.turn.token_usage.*`) out of the token columns.
- **A tool span takes its tool's name.** The span's original name moves to `attributes['cetra.span_name']`.
- **Placeholders mean no content.** `<REDACTED>` (Claude Code) and `[REDACTED]` (Codex) read as uncaptured content, like OpenInference's `__REDACTED__` (D-B1-10).
- **Codex's internal spans are discarded.** Codex exports every `tracing` span of the process: about 700 for a one-tool prompt (`fs.get_metadata`, `auth`, `append_items`, …), with no setting to filter them.
  - On spans detected as Codex, `normalize` keeps only the turn span and the tool dispatch spans, and discards the rest. Discarded spans are neither stored nor rejected, and not units; `normalize` counts them in `Normalized.discarded`.
  - Kept spans are re-linked by rule, because their real parents are discarded. The turn span's `span_id` becomes the first 8 bytes of SHA-256(`cetra.codex.turn` ‖ `turn.id`), and its parent is cleared. A tool span's parent becomes that same value, computed from its own `turn.id`. This needs no other span of the batch.
  - The turn span ends after its tools, so it is usually exported in a later batch. The rule above still links them.
- **Gemini CLI's process attributes are dropped.** Gemini puts the whole command line, prompt included, in the resource (`process.command_args`, `process.command`), even with `logPrompts` off. On its spans those keys, and `process.owner`, are removed from `resource_attributes`.
- **A subagent stays where the agent put it.** Claude Code nests a subagent's spans under the Agent tool's span. It is not split into its own trace.

The exact attribute names for Codex, and the open points for Gemini (the root span, the trace structure), are pinned by M1's captures. The M2 plan writes them into spec 02. Anything M1 contradicts here goes back to this document first.

### Session context (D-CA-8, D-CA-9)

**Route.** `POST /v1/agent-context`, on the same listeners as `/v1/traces` (the main port and 4318).
- Key-only, `ingest` scope, like the OTLP routes (D-G-15).
- `Content-Type: application/json`, body at most 4 KiB.
- Not counted as units, refused like OTLP when the org is over its units cap (`402`), and not rate-limited by the span rate. The 10,000-entry cap bounds what it stores.
- Documented in `openapi.json`, since it is our hook's contract.
- It joins `otlp::routes()` with `documented: true`, so it is served on 4318 and on the main port once each.
- No gRPC method: hooks speak HTTP.
- Spec 08's `ingest` scope gains "and the agent context".
- The server logs the project, the agent, the body size and the status, never the body's values (AGENTS.md § Privacy). A `400` names the field, never its value.

```json
{
  "agent": "claude-code",                 // claude-code | codex | gemini-cli | copilot
  "session_id": "…",                      // 1–256 chars, the id the agent puts on its spans
  "repository": "https://github.com/o/r.git", // optional, the origin URL without user-info; the server canonicalizes it (D-CA-4)
  "branch": "main",                       // optional, 1–256 chars
  "user": "dev@example.com"               // optional, 1–256 chars, lowercased by the server
}
```

- **Answers.** `204` on success, `400` for a malformed body, and `401`, `403` and `413` as for OTLP.
- **Merge.** A field that is absent leaves the stored value as it was. A field sent as `null` clears it.
- **Storage.** Last write wins per `(project, agent, session_id)`.
  - Each write goes to `meta.db` and to an in-memory map. The table is `agent_contexts`, with `project_id` a foreign key `ON DELETE CASCADE`.
  - The map is loaded from `meta.db` before the listeners open, like D-I-8's pending traces.
  - The ingest path reads only the map, never `meta.db` (spec 02 § Online-eval triggers, D-I-8).
  - **Skipping writes.** A post that changes nothing, onto an entry written less than an hour ago, writes nothing; otherwise `meta.db` and the map get the new `updated_at`. Expiry is measured from that write.
  - **Expiry.** An entry expires 30 days after its last write, or after the project's `retention_days` when that is shorter, so an email is not kept longer than the spans.
  - **Sweep.** An hourly sweep deletes expired entries from both stores. A lookup does not check expiry: at most one hour stale.
  - **Cap.** A project keeps at most 10,000 entries. Inserting a new key over the cap evicts the entry with the oldest `updated_at` from both stores.
- **Stamp.** The stamp happens inside `normalize`, so the D-CA-5 precedence lives in one place.
  - The server passes the project's `AgentContexts` as `normalize`'s fourth argument (§ Normalization API). `normalize` stays pure: the lookup is an input, like the price table. `IngestContext` stays `Copy` and unchanged.
  - For every span detected as an agent and with a `session_id`, `normalize` looks up `(coding_agent, session_id)`.
  - The context fills `repository` and `branch` over the span's own values (a hook knows the current branch).
  - It fills `user_id` at its D-CA-5 rank.
  - In-process commits (judge traces) pass an empty lookup.
  - Spans that arrive before the context are not rewritten later.
- **Deletion.**
  - A delete by user (track M) deletes the contexts whose `user` equals the deleted `user_id`. This is best-effort: a person whose spans carry another id keeps their context until it expires.
  - A deletion by trace or by filter leaves contexts alone.
  - A project drop deletes the rows (cascade) and the map's entries.
  - No path rewrites spans.
- **Tests.** The fixture format cannot carry contexts (`case.json` § `context`), so the stamp is tested by Rust unit tests in `cetra-otel` and integration tests in the server.

**Hook.** `cetra agents hook <agent>` is a hidden subcommand.
- It reads the agent's hook JSON on stdin: `session_id`, `cwd` and `hook_event_name`. All three agents send these, as the spike verified.
- It runs `git -C <cwd> remote get-url origin` and `git -C <cwd> rev-parse --abbrev-ref HEAD`, each with a 1 s timeout.
  - It removes the user-info from the URL before sending it.
  - It sends `null` for `repository` when there is no `origin`, for `branch` on a detached HEAD (`HEAD`), and for both outside a git repository or when git fails. A stale value is never kept.
- It posts the context with the endpoint and key from `agents.toml`. That file is in the same directory as the CLI's `credentials.toml` (spec 08 § CLI login), on Linux and macOS alike.
- It **always exits 0**, **never writes to stdout**, gives up after 2 s in total, and logs failures to stderr without payload content.
- `user` comes from `agents.toml` (`--user` at setup, default `git config --global user.email`).
- **How hooks call it.** Setup writes the absolute path of the running `cetra` binary (`std::env::current_exe`) into the hook command, because hooks may run without the user's `PATH`. `remove` recognizes our hooks as commands that end with `agents hook <agent>`.

It runs at:
- **Claude Code:** `SessionStart`, `UserPromptSubmit` and `CwdChanged`.
- **Codex:** `SessionStart` and `UserPromptSubmit`, in `~/.codex/hooks.json`. Hooks in `config.toml` tables did not run in the spike. Codex runs a hook only once the user has trusted it (`[hooks.state]` with a hash Codex computes). So the docs tell the user to approve the cetra hook in Codex once, and until then Codex traces have no repository or branch.
- **Gemini CLI:** `SessionStart` and `BeforeAgent`.
- **Copilot:** none in the MVP. VS Code already sends `github.copilot.git.*`, and Copilot CLI's hooks are unverified.

## Setup command (D-CA-10)

```
cetra agents setup <claude-code|codex|gemini-cli|copilot> --url <server>
                   [--user <email>] [--no-content] [--print]      # key from CETRA_INGEST_KEY or --key
cetra agents remove <agent>
```

**The key.** The command needs a project key with the `ingest` scope. The documented way is the new variable `CETRA_INGEST_KEY`; `--key` exists but shows in `ps`, as `cetra query --token` warns (`crates/cli/src/client.rs`). A PAT cannot create keys: key creation is session-only (D-P-70). So the user creates the key in the project's API keys page. The docs page shows the whole command.

**What it writes, per agent.** It writes **user-level** files only, because the agents refuse exporter settings from repository files.

| Agent | File | Contents |
|---|---|---|
| Claude Code | `~/.claude/settings.json` | `env`: `CLAUDE_CODE_ENABLE_TELEMETRY=1`, `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`, `OTEL_TRACES_EXPORTER=otlp`, `OTEL_LOGS_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` (`Authorization=Bearer%20<key>`), `OTEL_METRICS_INCLUDE_REPOSITORY=true` (the spike found `vcs.*` on spans with it); with content: `OTEL_LOG_USER_PROMPTS=1`, `OTEL_LOG_TOOL_DETAILS=1`, `OTEL_LOG_TOOL_CONTENT=1`. `hooks`: the three events above |
| Codex | `~/.codex/config.toml` and `~/.codex/hooks.json` | `[otel]` `trace_exporter = { otlp-http = { endpoint = "<url>/v1/traces", protocol = "binary", headers = { authorization = "Bearer <key>" } } }` (the full path), `exporter = "none"`, `metrics_exporter = "none"`; content is not on Codex spans, so `--no-content` changes nothing here; the hooks in `hooks.json` |
| Gemini CLI | `~/.gemini/settings.json` | `telemetry`: `enabled`, `traces`, `target = "local"`, `otlpEndpoint = "<url>"` (a base URL: Gemini appends `/v1/traces`, `/v1/logs` and `/v1/metrics`), `otlpProtocol = "http"` (sends JSON), `logPrompts` per content; hooks. The key goes in `~/.gemini/.env` as `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>` (mode 0600): the spike showed Gemini ignores `telemetry.otlpHeaders` and reads that file. Gemini always sends logs and metrics too, which cetra drops |
| Copilot | none | prints the environment variables for the shell profile and the VS Code settings, including `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY` with content |

**Switching off logs and metrics.** Setup sends no logs or metrics. cetra drops them (D-G-40), so sending them would only cost the developer's bandwidth. For Codex, this also stops the default metrics export to OpenAI's Statsig.

**Content.**
- On by default, like the SDKs' `capture_content=true` (spec 10). It covers prompts, tool arguments and tool output.
- Raw API bodies are never turned on.
- `--no-content` keeps every content opt-in off.

**Editing the user's files.**
- Edits are idempotent and keep every other key. TOML is edited in place with `toml_edit`, which keeps comments.
- The original file is copied to `<file>.cetra-backup` once, before the first change.
- `remove` deletes only the keys and hooks that setup wrote. It recognizes them by their values: our endpoint, and hook commands starting with `cetra agents hook`.
- `--print` prints the snippet and writes nothing.

**The hook's config.** `~/.config/cetra/agents.toml` (mode 0600) holds `url`, `key` and `user`. The agents' own settings files hold the key too, in their OTLP headers, since that is the only way they send it.

## Cost (D-CA-7)

- Model calls are priced by the project's price table (spec 02 § Cost computation). The agents' own figures are not read: Claude Code's and Codex's are in logs and metrics, and Copilot's `github.copilot.cost` is a billing multiplier, not dollars.
- The models these agents use are in the LiteLLM snapshot. A model without a price is `cost_source = none`, as everywhere, and a custom price fixes it (track T).
- **For a subscription user (Claude Max, ChatGPT plans, Copilot), the figure is labelled "API-equivalent cost" (D-CA-15): what the same tokens would cost at API prices, not what the user pays.** The docs page and the dashboard template's description say so.

## Dashboard template (D-CA-11)

"Coding agents" joins the templates of D-C-21: Blank, Cost, Latency, Quality. Like them, it is defined in the UI and copied into a new dashboard. Every widget filters on `metadata['coding_agent'] IS NOT NULL`.

**Number widgets:** total cost, sessions (`count_distinct session_id`), people (`count_distinct user_id`), turns (traces), and tokens.

**Over time:**
- cost by agent, a stacked area on `metadata['coding_agent']`;
- cost by model (spans, `llm`).

**Horizontal bars, top 10 by cost:**
- people (`user_id`);
- repositories (`metadata['repository']`);
- branches (`metadata['branch']`);
- sessions (`session_id`, which needs the new breakdown field below).

**Tables:**
- tool calls by tool name: spans of kind `tool`, with count, error rate and p95 duration;
- cost per turn by agent: p50 and p95.

**The new breakdown field.** `session_id` joins the `traces` breakdown fields of spec 05. The sessions list already sorts by cost, so a top-sessions widget is the template's only use for it. Breakdown limits stay within 1–20.

**No other UI.**
- No new page and no new facet: the facet sidebar already lists `metadata` keys (spec 11 § Facet sidebar).
- The trace view, the sessions page (one prompt per turn) and the users page work as they are.
- Without content opt-ins a turn shows no prompt.
- Without the logs track, a Claude Code turn never shows the answer.

**Units.** A Claude Code prompt makes many spans: the turn, every model call, and each tool with its two children. Every span is a unit (D173). The docs page says so, with a measured example from M1's captures.

## Decisions

The decisions are recorded in `docs/mvp-decisions.md` § Track CA. Here are the reasons and the alternatives that were rejected.

| ID | Decision | Rejected, and why |
|---|---|---|
| D-CA-1 | **Agents: Claude Code, Codex CLI, Gemini CLI, Copilot (CLI and VS Code)** (user). | Cursor: no spans (§ Non-goals). |
| D-CA-2 | **Spans only; D-G-40 stands** (user). | Logs folded into spans, or logs and metrics with a session aggregate (LangWatch): a new storage path and a much larger track. Its absence costs Claude Code's answer text and reported cost, Codex's content, lines, commits and PRs, and Cursor. |
| D-CA-3 | **Agents are a detection result in `cetra-otel`, not new `Convention` values.** The result comes from `service.name`, the scope, the span name and the attributes (§ Agent detection). Per-agent rules run after the generic extraction and before cost. | New `Convention` variants: a `cetra-core` contract change (snapshot, OpenAPI, web types), while `conventions` is only a debug aid. Reading Claude Code's token names (`input_tokens`, …) on every span: they are too generic. |
| D-CA-4 | **`metadata` keys `coding_agent`, `repository`, `branch`.** | New columns in `spans` and `traces`: a `cetra-core` and storage schema change for three strings that `metadata` already filters, facets and breaks down. Semconv keys (`vcs.repository.url.full`, `vcs.ref.head.name`): long to type in filters, and the URL is not the canonical form. A `cetra.` prefix: noise in every filter. A `cwd` key: a home path names the person, and the repository covers the use. |
| D-CA-5 | **`user_id` is the person** (account email, then the hook's user, then account ids, then the generic sources). | Keeping the generic precedence: Claude Code's `user.id` is a random install id, which makes the users page useless. |
| D-CA-6 | **Span shapes**: turns are `agent`, tool spans are named after the tool (the original name goes to `attributes['cetra.span_name']`), and the redaction placeholders mean no content. | Keeping `claude_code.tool` as every tool's name: the trace tree would be unreadable. |
| D-CA-7 | **Cost from our price table.** The agents' own figures are not read. The cost is the API-price equivalent. | Reading the agents' cost: it is on logs and metrics only (D-CA-2), and Copilot's is not dollars. |
| D-CA-8 | **A hook in the `cetra` binary** (user: native OTel plus a small hook). It sends context only, never spans, and never reads transcripts. | No hook: no branch, and no person for API-key users. A full hook tracer (MLflow, Phoenix): coupled to unversioned transcripts, with reconstructed timing. An npm or pip hook package: one more runtime on the developer's machine, and two more packages to release. |
| D-CA-9 | **Server-side session context**, last write wins per `(project, agent, session_id)`, passed to `normalize` as its fourth argument, expiring after 30 days or the project's retention when shorter. | The hook emitting a context span: it would add a junk trace to every session, since hooks do not know the turn's trace id. `OTEL_RESOURCE_ATTRIBUTES` from a shell wrapper: fixed at launch, so a branch switched mid-session is never seen, and every launch path would need wrapping. Joining at query time: storage cannot join `meta.db` in widget queries. |
| D-CA-10 | **`cetra agents setup` writes the agents' user-level settings.** It needs an ingest key (D-P-70). Content on by default; logs and metrics off. | Docs-only setup: about 12 variables per agent, easy to get wrong. Turning on raw API bodies (LangWatch): whole conversations in every export, which LangWatch then had to cap. |
| D-CA-11 | **The "Coding agents" dashboard template, plus `session_id` as a traces breakdown** (user: a template, no page). | A dedicated page, or cost per PR: post-MVP. |
| D-CA-12 | **Fixtures captured from the real agent CLIs** against `tools/capture`'s mock provider: Claude Code through `ANTHROPIC_BASE_URL`, Codex through a `model_providers` entry, Gemini CLI through its base-URL variable. Each agent version is pinned in the scenario. New fixture groups: `claude_code`, `codex`, `gemini_cli`. Copilot has no base-URL override, so its case is hand-written from GitHub's attribute reference, in group `genai`, since its spans are plain GenAI semconv. | Hand-writing every case: the agents' span shapes are beta and undocumented in places, and a capture is the only way to pin them. |
| D-CA-13 | **The Claude Agent SDK needs nothing of its own.** It injects `TRACEPARENT`, so a Claude Code turn nests under the application's span in the same trace, and normalizes the same way. The integrations page documents it. | — |

## Risks and what M1 must settle

- **Beta span shapes.** Claude Code's spans are beta, and the docs say names may change. Codex spans come from `tracing` and are undocumented. A capture pins one version of each. The docs page names the versions tested, and a changed shape is a new capture plus a rule. These conventions get no compatibility code for older agent versions (AGENTS.md § Rules): the fixtures cover the pinned versions only.
- **Session ids.** Verified for Claude Code (`session.id`) and Gemini CLI (`gen_ai.conversation.id`): they equal the hook's `session_id`. For Codex, `conversation.id` equals the session id that `codex exec` prints, but its hooks did not run untrusted, so the equality with the hook payload is unverified. M1 checks Codex (`thread.id` / `conversation.id` against the hook's `session_id`) and Gemini CLI (`gen_ai.conversation.id` / `session.id`). If one differs, that agent's hook is dropped from the MVP, and its context comes only from its own attributes.
- **Trace boundaries.**
  - Claude Code: one trace per prompt.
  - Codex (`codex exec`): one trace per turn, rooted at the `turn/start` request.
  - Gemini CLI: one trace per model call or tool batch.
  - Interactive Codex was not captured. If its turns share one long trace, the re-linking rule still gives one root span per turn, but the trace holds the whole session. The docs name the tested modes.
- **Protocols.**
  - Gemini CLI's `otlpProtocol=http` is either protobuf or JSON, and cetra accepts both.
  - Copilot CLI refuses `http://` endpoints, and `cetra serve` has no TLS. A self-hosted cetra therefore needs TLS in front of it for Copilot CLI: the docs page documents a reverse proxy (Caddy or nginx) with TLS, and `cetra serve` gets no TLS (D-CA-16). VS Code Copilot and cetra cloud are not affected.
- **Codex's cost is per turn.** Pricing the turn's summed tokens differs from pricing each call only when a long-context tier applies to some calls and not others. This is accepted.
- **Gemini's output tokens.** Its spans carry only `gen_ai.usage.input_tokens` and `output_tokens`. Whether `output_tokens` includes the thoughts is unverified, since the mock's thoughts did not show on the span. Cache and reasoning counts are absent.


## Milestones

The milestones are cut so that four can start at once, each in its own worktree and branch from this design's commit. Their files do not overlap, except for generated files (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`, the CLI reference page). Those are regenerated after each merge, never hand-merged.

| M | Delivers | Depends on | Owns | Leaves out |
|---|---|---|---|---|
| **M1 Captures** | The spike is done (research § 6). Delivers:<ul><li>A TypeScript scenario per agent whose `main.ts` runs a pinned agent CLI (Claude Code `-p`, `codex exec`, `gemini -p`) headless against the mock provider, with a scripted tool call. The pins are npm packages in `tools/capture/ts`'s lockfile.</li><li>Scripted turns that cover every model call the agent makes, in order: Claude Code 2 (`?beta=true`), Codex 2 (Responses, tool `exec_command`), Gemini CLI 3 (a JSON routing reply, then 2 streamed calls).</li><li>A scenario option `span_names`: an allowlist applied by the capture tool before writing, so that Codex's ~700 spans fit in 256 KiB. It keeps the turn, the tool dispatch spans and a few named noise spans, to exercise the discard rule.</li><li>`GROUPS` (scenario.rs, `fixtures/otlp/README.md`, `crates/otel/tests/support`) gain the agents.</li><li>Scenario options `volatile_attributes` (random ids re-minted, so recaptures compare equal).</li><li>No real identity lands in a fixture: each agent runs with an isolated home and a fake git identity, and the scrubber's resource and path rules and the fixtures secret scan apply.</li><li>Fixture inputs awaiting bless, in the groups `claude_code`, `codex`, `gemini_cli`, within the 256 KiB and 10 s limits.</li><li>The Copilot case, hand-written in `genai` (D-CA-12).</li><li>`docs/research/coding-agents.md` § 6, with the answers to § Risks.</li></ul>`just check-capture` includes the agent scenarios. If an agent cannot run headless without a real account, M1 stops and reports. | — | `tools/capture/`, `fixtures/otlp/` (`input.json`, `case.json`, README), `crates/otel/tests/support` (`GROUPS` only), the research doc | Normalization: no `expected.json` for the new groups |
| **M2 Normalization** | Spec 02 (and spec 01's `metadata` keys and `cetra.span_name`) rules for the four agents: detection, kinds, tokens, tool names, payloads, placeholders, the person override, the D-CA-4 keys from the agents' own attributes, and the stamp from an `AgentContexts` lookup (§ Normalization API). `schema_version` is bumped. Every caller of `normalize` passes the lookup; the server passes an empty one until M3. The M1 fixtures are blessed and reviewed. | M1 | `crates/otel/`, the callers' one-line changes in `crates/eval` and `crates/server/src/ingest`, `fixtures/otlp/` (`expected.json`), spec 02 | The server store |
| **M3 Session context** | Phases 1–n (no M2 needed): the `agent_contexts` migration, the store with expiry and the per-project cap, `POST /v1/agent-context`, the deletion cascade (delete by user, project drop), spec 04 and spec 09. Specs 04 (table), 08 (`ingest` scope), 09 (route) and 12 (startup load). Last phase (after M2 merges): the store keeps one `AgentContexts` per project, `accept_request` passes it to `normalize`, the route canonicalizes `repository` with `cetra_otel::canonical_repository`, and a server test shows a context stamped onto later spans with the D-CA-5 order. | design; M2 for the last phase | `crates/server/` (not `ingest/` before the last phase), `crates/server/migrations/`, specs 04 and 09 | The CLI |
| **M4 CLI** | `cetra agents hook`, `setup` and `remove` for the four agents (D-CA-8, D-CA-10), `agents.toml`, in-place JSON and TOML edits with the backup, `--print` and `--no-content`, tests on temporary home directories and against a stub HTTP server, the regenerated CLI reference, spec 09 § CLI, `CETRA_INGEST_KEY` in the configuration reference. `toml_edit` is pinned in the root `[workspace.dependencies]` after `cargo info`. | the route contract in this document | `crates/cli/`, the root `Cargo.toml` and `Cargo.lock`, the CLI reference page | — |
| **M5a Dashboard** | `session_id` as a `traces` breakdown field (spec 05, `crates/storage`, OpenAPI). The "Coding agents" template (D-CA-11) in `web/`, with its tests on the MSW mocks. Spec 11. | design | `crates/storage/` dashboard query code, `web/`, specs 05 and 11 | — |
| **M5b Docs and end-to-end** | Docs pages under `website/src/content/docs/docs/integrations/` for Claude Code (with the Agent SDK; named `claude-code.mdx`, apart from the existing `anthropic.mdx`), Codex, Gemini CLI and Copilot, and the integrations index. They pass the site's checks: backing fixtures, tested or untested samples, env vars. They also cover the units note and the API-price equivalent. A `CHANGELOG.md` line. An end-to-end test in `cetra serve`: replay a captured Claude Code export and a context post, then check the session, the person, the repository and a template widget's query. Also an Agent SDK case: a Claude Code turn under an application root. The roadmap row is marked done. | M1–M5a (M3 directly, and through M4) | `website/`, `CHANGELOG.md`, `crates/cli/tests/` (e2e), `docs/roadmap.md` | — |

## Normalization API

M2 adds this to `cetra-otel` (`crates/otel/src/agent_context.rs`, re-exported from `lib.rs`). M3 builds on it.

```rust
/// One coding-agent session's context (D-CA-9), as the server stores it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentContext {
    /// Canonical form of D-CA-4 (`canonical_repository`), or `None`.
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub user: Option<String>,
    /// Last write, ns since the Unix epoch. `normalize` ignores it; the server's expiry and cap use it.
    pub updated_at: i64,
}

/// The contexts of one project, keyed by (agent, session id). `normalize` only reads it.
#[derive(Debug, Clone, Default)]
pub struct AgentContexts { /* HashMap<(String, String), AgentContext> */ }

impl AgentContexts {
    pub fn get(&self, agent: &str, session_id: &str) -> Option<&AgentContext>;
    pub fn insert(&mut self, agent: &str, session_id: &str, context: AgentContext);
    pub fn remove(&mut self, agent: &str, session_id: &str) -> Option<AgentContext>;
    pub fn retain(&mut self, keep: impl FnMut(&str, &str, &AgentContext) -> bool);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

/// `https://user:tok@GitHub.com:443/O/R.git/` → `github.com/o/r`; `git@github.com:o/r.git` → `github.com/o/r`;
/// `ssh://git@host/o/r` → `host/o/r`. `None` when no host and path remain.
pub fn canonical_repository(remote: &str) -> Option<String>;

pub fn normalize(
    request: &ExportTraceServiceRequest,
    ctx: &IngestContext,
    prices: &crate::cost::PriceTable,
    agents: &AgentContexts,
) -> Normalized;
```

**The server's side.**
- It keeps one `Arc<std::sync::RwLock<AgentContexts>>` per project.
- `accept_request` runs normalization in `spawn_blocking`, so it clones the `Arc` into the task and holds the read lock for that one `normalize` call. HTTP and gRPC share `accept_request`, so this is one call site.
- Writes come only from the context route and the sweep, so contention is negligible.
- `cetra-eval`, the fuzz entry and every test pass `&AgentContexts::default()`.
- The reserved-environment rewrite (D-G-7) stays where it is, in `accept_request` after `normalize`.

**What the hook sends.** `repository` is the `origin` URL with its user-info removed, never the raw URL with a token. The server canonicalizes it, so the CLI does not depend on `cetra-otel` (AGENTS.md § Crate dependency graph).

## Answered questions

The user answered the four questions on 2026-11-20:

1. Content is on by default in `cetra agents setup`; `--no-content` turns it off; raw API bodies are never captured (D-CA-14).
2. The subscription figure is labelled "API-equivalent cost" in the template and the docs (D-CA-15).
3. Copilot CLI on a self-hosted instance: a reverse proxy with TLS in front of cetra is documented; `cetra serve` gets no TLS (D-CA-16).
4. Qwen Code joins the MVP if its captured spans match Gemini CLI's; another session decides that from the M1 capture (D-CA-17).
