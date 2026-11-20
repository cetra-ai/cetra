# Coding agents: what they emit, how competitors ingest it, and what cetra accepts (Nov 2026)

Research for track CA (D195). It answers four questions before any design:
- what each coding agent emits today;
- how the competitors turn that into traces;
- what `cetra-otel` and the server accept;
- what normalization is missing.

Sources:
- Primary docs and the agents' own repositories, read on 2026-11-20. Versions seen: Claude Code 2.1.329, Codex `rust-v0.171.0`, Gemini CLI v0.70.0, opencode v1.19.6, Cline v4.1.26.
- The competitors' code in `/home/sebastien/github/{langwatch,opik,mlflow,phoenix,langfuse,helicone,lmnr}`.

Anything marked *unverified* was not confirmed from a primary source, and must be captured before the design relies on it.

## 1. Summary

**Spans, logs and metrics.**
- Every agent worth supporting can now export **OTLP spans**. Claude Code's spans are beta, behind `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`. Codex, Gemini CLI, Copilot (CLI and VS Code), opencode and Cline also export spans.
- Cursor is the exception. Its export runs on Cursor's servers, on the Enterprise plan only, and sends **metrics and logs, no spans**.
- The richest data is not on the spans. Claude Code puts its cost, its assistant text and repository identity on **log events**. It also puts lines of code, commits and PR counts on **metrics**. Codex keeps all content (prompt, tool arguments and output) in logs only.

**Our side.**
- cetra stores spans only. `/v1/logs` and `/v1/metrics` are accepted and dropped (D-G-40).
- Our normalizer already handles the GenAI-semconv agents: Copilot completely, Gemini CLI almost.
- It does not understand Claude Code's own token names, Codex's span shapes, or Gemini's operation names.

**Competitors.**
- Only LangWatch ingests all three signals and builds a coding-agent product: sessions, cost per PR, and a GitHub branch-to-PR join.
- Opik, MLflow, Phoenix and Langfuse take spans only. Some add a hook plugin that reads the agent's transcript, or a proxy.
- Nobody but LangWatch has cost per PR. Nobody but LangWatch and Opik's paid Cost Intelligence has cost per user or per repository for coding agents.

## 2. What each agent emits

### 2.1 Claude Code

Source: code.claude.com/docs/en/monitoring-usage, /hooks, /agent-sdk/observability.

**Switches.**
- `CLAUDE_CODE_ENABLE_TELEMETRY=1` turns telemetry on.
- `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER` and `OTEL_TRACES_EXPORTER` each take `otlp`.
- Spans also need `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`.
- `OTEL_EXPORTER_OTLP_PROTOCOL` takes `grpc`, `http/protobuf` (the default for http endpoints since v2.1.323) or `http/json`.

**Endpoints, headers and resource.**
- Standard `OTEL_EXPORTER_OTLP_*` endpoints and headers, including per-signal ones.
- `otelHeadersHelper` is a settings key: a script that prints the headers. It is http only.
- `service.name` is `claude-code`. `OTEL_RESOURCE_ATTRIBUTES` is honoured.

**Where it can be configured.**
- The variables are set in the user's `~/.claude/settings.json` `env`, in managed settings, or in the shell.
- A repository's `.claude/settings.json` cannot set an exporter.
- `OTEL_*` variables are not passed to the Bash tool, hooks or MCP servers.

**Export intervals.** Logs and traces every 5 s, metrics every 60 s, with delta temporality.

**Standard attributes.** They are on every metric, event and span.

| Attribute | When |
|---|---|
| `session.id` | default on (`OTEL_METRICS_INCLUDE_SESSION_ID`); `/clear` starts a new one |
| `user.id` | always; an anonymous random id from `~/.claude.json` |
| `user.email` | when signed in |
| `user.account_uuid`, `user.account_id` | default on |
| `organization.id` | when signed in |
| `terminal.type` | when detected |
| `app.version` | opt-in |
| `app.entrypoint` (`cli`, `sdk-py`, `sdk-ts`, `claude-vscode`, …) | opt-in |
| `vcs.repository.url.full`, `vcs.owner.name`, `vcs.repository.name`, `vcs.provider.name` | opt-in, `OTEL_METRICS_INCLUDE_REPOSITORY=true` (v2.1.269+): from the `origin` remote, lowercased, credentials stripped. *Unverified* whether spans carry them, or only metrics and events |

**Spans (beta).** The docs warn that names and attributes may change.

```
claude_code.interaction            one per user prompt (the trace root)
├─ claude_code.llm_request
├─ claude_code.hook                (detailed beta only)
└─ claude_code.tool
   ├─ claude_code.tool.blocked_on_user
   └─ claude_code.tool.execution
```

A subagent's `llm_request` and `tool` spans nest under the parent's `claude_code.tool` span for the Agent tool. Every span carries `span.type`.

`claude_code.interaction`:
- `user_prompt`, which is `<REDACTED>` unless `OTEL_LOG_USER_PROMPTS=1`;
- `user_prompt_length`, `interaction.sequence`, `interaction.duration_ms`.

`claude_code.llm_request`:
- model: `model`, `gen_ai.system=anthropic`, `gen_ai.request.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons`, `stop_reason`;
- tokens: `input_tokens` (**excludes cache**), `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`. These are Claude Code's own names, not `gen_ai.usage.*`;
- timing: `duration_ms`, `ttft_ms`;
- request and outcome: `request_id`, `attempt`, `success`, `status_code`, `error`;
- agent context: `query_source` (`main`, `subagent`, `auxiliary`, …), `agent_id`, `parent_agent_id`, `speed`, `effort`.

There is **no cost and no output text** on this span.

`claude_code.tool`:
- `tool_name`, `tool_use_id`, `gen_ai.tool.call.id`, `duration_ms`, `result_tokens`;
- with `OTEL_LOG_TOOL_DETAILS=1`: `file_path`, `full_command`, `skill_name`, `subagent_type`;
- with `OTEL_LOG_TOOL_CONTENT=1`: a `tool.output` span event with `content`, `output`, `diff`, `file_path` and `bash_command`, cut at 60 K UTF-16 units.

**Events (OTLP log records, `claude_code.<name>`).** Each carries the standard attributes, `event.name`, `event.timestamp`, `event.sequence` and `prompt.id`. `prompt.id` links all events of one user prompt; it is events-only.

The ones that matter:
- `user_prompt`: `prompt_length`, and `prompt` when opted in.
- `assistant_response`: `response`, opt-in through `OTEL_LOG_ASSISTANT_RESPONSES` (falling back to the prompts flag), plus `model` and `request_id`.
- `api_request`:
  - `model`, `cost_usd`, `cost_usd_micros`, `duration_ms`;
  - the four token counts, plus `cache_creation_5m_tokens` and `cache_creation_1h_tokens`;
  - `request_id`, `query_source`.
- `api_error`, `api_refusal`, `api_retries_exhausted`.
- `tool_result`:
  - `tool_name`, `tool_use_id`, `success`, `duration_ms`, `decision_source`, and the input and result sizes;
  - with `OTEL_LOG_TOOL_DETAILS=1`: `tool_parameters`, `tool_input`, and on a successful `git commit`, `vcs.ref.head.name` and `vcs.ref.head.revision`.
- `tool_decision`: `decision`, `source`, `permission_mode`.
- `compaction`: `pre_tokens`, `post_tokens`.
- `subagent_completed`.
- `api_request_body` and `api_response_body`: opt-in through `OTEL_LOG_RAW_API_BODIES`; the full conversation, up to 60 KB inline.

Without `TRACEPARENT` the events carry **no trace or span id**. They join spans only through `prompt.id`, `request_id` and `tool_use_id`, and `prompt.id` is not on the spans.

**Metrics.**
- `claude_code.session.count`.
- `claude_code.cost.usage` (USD) and `claude_code.token.usage` (`type` = `input`, `output`, `cacheRead`, `cacheCreation`), both by `model`.
- `claude_code.lines_of_code.count` (`added`, `removed`).
- `claude_code.commit.count` and `claude_code.pull_request.count`.
- `claude_code.code_edit_tool.decision`.
- `claude_code.active_time.total`.

Metrics carry `session.id` and no trace context.

**Content opt-ins** (all off by default): `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_ASSISTANT_RESPONSES`, `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_TOOL_CONTENT` and `OTEL_LOG_RAW_API_BODIES`. Assistant text reaches a span only through "detailed beta tracing" (`ENABLE_BETA_TRACING_DETAILED` + `BETA_TRACING_ENDPOINT`). That needs an allowlisted org in interactive mode, and it redirects logs and traces to that endpoint.

**What is missing.** There is no cwd attribute and no PR identifier. The branch appears only on commit `tool_result` events. `OTEL_RESOURCE_ATTRIBUTES` can carry anything the launcher sets, such as `vcs.ref.head.name`, but it is fixed for the process.

**Hooks.** Hooks are declared in settings and can ship inside a **plugin** (`hooks/hooks.json`). Their types are `command`, `http` (POSTs the stdin JSON), `prompt`, `mcp_tool` and `agent`.
- There are 33 events. The useful ones:
  - `SessionStart` (`source`, `model`) and `SessionEnd`;
  - `UserPromptSubmit` (`prompt`);
  - `PreToolUse`, and `PostToolUse` (`tool_input`, `tool_response`, `duration_ms`);
  - `PostToolUseFailure`;
  - `SubagentStart`, and `SubagentStop` (`agent_transcript_path`);
  - `Stop` (`last_assistant_message`, no usage);
  - `PreCompact`, `CwdChanged` and `WorktreeCreate`.
- Every payload has `session_id`, `prompt_id`, `transcript_path`, `cwd` and `permission_mode`.

**Transcript.** `~/.claude/projects/<project>/<session>.jsonl`. The docs call it internal and liable to change.
- Assistant lines carry `message.model` and `message.usage`.
- *Unverified*: one turn is written as several lines sharing `message.id`, with the usage repeated on each.

**Agent SDK.** It spawns the CLI, so its telemetry is the same, with `app.entrypoint=sdk-py|sdk-ts`. It injects `TRACEPARENT` from the caller's active span, so a Claude Code interaction nests under the application's own trace.

### 2.2 OpenAI Codex CLI

Source: `codex-rs/otel` (`events/session_telemetry.rs`, `metrics/names.rs`, `config.rs`), `codex-rs/config/src/types.rs`, learn.chatgpt.com/docs/config-file/config-advanced, /docs/hooks.

**Config.** The `[otel]` table of `~/.codex/config.toml`. A repository's config cannot set it.
- `exporter` sends logs: `otlp-http` (`protocol = "binary" | "json"`, with the full `/v1/logs` endpoint) or `otlp-grpc`.
- `trace_exporter` sends spans.
- `metrics_exporter` defaults to `statsig`, which is OpenAI's own collector.
- `log_user_prompt` and `log_agent_responses` are off by default.
- Also: `headers_helper`, `[otel.span_attributes]` (extra attributes on every span) and `environment`.
- *Unverified*: whether `trace_exporter` appends `/v1/traces` to its endpoint.

**Spans.** They come from Rust `tracing`, renamed by `otel.name`, and they are **not GenAI-semconv shaped**.
- `turn` / `session_task.turn`:
  - `thread.id`, `turn.id`, `model`;
  - `codex.turn.token_usage.{input_tokens, cached_input_tokens, cache_write_input_tokens, non_cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`.
- `handle_responses`, renamed to the response event type (for example `completed`):
  - `gen_ai.usage.input_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`, `gen_ai.usage.output_tokens`;
  - `codex.usage.reasoning_output_tokens`, `codex.usage.total_tokens`.
- Tool dispatch spans named after the tool.
- `codex.exec`, which is the root span of `codex exec`.

Spans carry lengths, never content.

**Events (logs).** Every event has `conversation.id`, `user.account_id`, `user.email`, `model`, `app.version` and `terminal.type`.
- `codex.conversation_starts`
- `codex.user_prompt`: `prompt`, which is `[REDACTED]` unless opted in.
- `codex.api_request`
- `codex.sse_event`: tokens on `response.completed`.
- `codex.tool_decision`
- `codex.tool_result`: `arguments` and `output`, truncated. These are *apparently on by default*.
- `codex.turn_cost`: `usage.estimated_usd`.
- `codex.agent_response`: opt-in.

**Metrics.** They include `codex.turn.token_usage`, `codex.turn.cost_microusd` and `codex.tool.call`.

**Identifiers.** `conversation.id`, `turn.id`, the account id and email. There is no cwd, repository or branch in OTel.

**Hooks.**
- `notify`: one program run at `agent-turn-complete`, with `thread-id`, `turn-id`, `cwd` and the turn's messages.
- Lifecycle hooks in `hooks.json` or `config.toml`: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreModelRequest`, `PostModelRequest` (`token_usage`), `Stop`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact` and `Interrupt`.
- Each payload carries `session_id`, `transcript_path`, `cwd` and `model`.

**Rollout files.** `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
- `session_meta` holds `cwd` and `cli_version`.
- `TokenCount` events carry the usage.

### 2.3 Gemini CLI (and Qwen Code)

Source: `docs/cli/telemetry.md`, `docs/hooks/reference.md`, `packages/core/src/telemetry/`.

**Config.** `telemetry` in `.gemini/settings.json`, or the `GEMINI_TELEMETRY_*` variables:
- `enabled` and `traces` (both off by default);
- `target=local`;
- `otlpEndpoint`, which defaults to `localhost:4317`;
- `otlpProtocol`: `grpc` (the default) or `http`. *Unverified*: whether `http` is protobuf or JSON, and whether per-signal paths are appended;
- `otlpHeaders`;
- `logPrompts`, which **defaults to true**.

**Spans.** The span name is the operation, which is also `gen_ai.operation.name`: `user_prompt`, `system_prompt`, `llm_call`, `tool_call`, `agent_call` and `schedule_tool_calls`. Attributes:
- conversation and agent: `gen_ai.conversation.id` (the session), `gen_ai.agent.name=gemini-cli`;
- model: `gen_ai.request.model`, `gen_ai.response.model`;
- content: `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`, `gen_ai.tool.definitions`;
- tools: `gen_ai.tool.name`, `gen_ai.tool.call_id` (*sic*, not `call.id`);
- usage: `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`.

These are GenAI semconv apart from the operation names.

**Logs.** `gemini_cli.*` events (`user_prompt`, `tool_call`, `api_request`, `api_response` with five token counts, `api_error`, `file_operation`, …). Also the semconv `gen_ai.client.inference.operation.details` log.

**Metrics.** `gemini_cli.token.usage`, `gemini_cli.lines.changed`, `gemini_cli.tool.call.count`, and others. Gemini exports **no cost**.

**Identifiers.** `session.id`, `installation.id`, `user.email`. There is no cwd or repository.

**Hooks.** `BeforeAgent`, `AfterAgent`, `BeforeModel`, `AfterModel` (`llm_request`, `llm_response`), `BeforeTool`, `AfterTool`, `SessionStart`, `SessionEnd`, … Each payload has `session_id`, `transcript_path` and `cwd`.

**Qwen Code.** A fork with the same shape: `qwen-code.*` events, `QWEN_TELEMETRY_*` variables, and per-signal endpoints.

### 2.4 GitHub Copilot

**Copilot CLI and the shared runtime.**

Source: docs.github.com … /opentelemetry-reference.

Config:
- `COPILOT_OTEL_ENABLED=true`, or an `OTEL_EXPORTER_OTLP_ENDPOINT`;
- `OTEL_EXPORTER_OTLP_PROTOCOL` is `http/json` (the default) or `http/protobuf`; there is **no gRPC**;
- **a plain `http://` endpoint disables export**, so the endpoint must be https. This matters for a self-hosted cetra on localhost.

It sends spans and metrics, and no logs. The spans are GenAI semconv:
- `invoke_agent` is the root. It carries the turn's total usage, `gen_ai.conversation.id`, `enduser.pseudo.id`, `github.copilot.cost` (a billing multiplier, not USD) and `github.copilot.nano_aiu`.
- `chat {model}` carries the usage, `gen_ai.response.model` and `gen_ai.response.time_to_first_chunk`.
- `execute_tool` and `plan`.

Content is opt-in through `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY`. It covers `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result`.

**VS Code Copilot Chat.**

Source: code.visualstudio.com/docs/agents/guides/monitoring-agents.

Config: `github.copilot.chat.otel.*` settings, or the `COPILOT_OTEL_*` variables. It supports gRPC and HTTP.

The spans are the same shapes, plus `execute_hook`. Attributes:
- repository: `github.copilot.git.repository`, `github.copilot.git.branch`, `github.copilot.git.commit_sha`;
- identity, opt-in through `captureIdentity`: `user.name`.

Content is opt-in through `captureContent`.

### 2.5 Cursor

Source: cursor.com/docs/enterprise/opentelemetry-export (and /wire), /docs/hooks, /docs/account/teams/analytics-api.

**Export.**
- The export runs **server-side**. It is configured in Team Settings and is **Enterprise only**.
- It posts **OTLP/HTTP protobuf** to a public HTTPS base URL, reachable from Cursor's IPs, with `/v1/metrics` and `/v1/logs` appended. The resource is `service.name=cursor`.
- **No spans.**
- Metrics: `cursor.token.usage`, `cursor.cost.usage` (USD, an estimate) and `cursor.tool.calls`.
- Logs:
  - `cursor.api.request`, with tokens and the model;
  - `cursor.conversation.user_message`, `cursor.conversation.assistant_message` and `cursor.conversation.tool_io`, which are opt-in per team and per destination.
- Logs carry `cursor.conversation.id`, `cursor.user.email` and `cursor.project.name`.
- Delivery is at-least-once (dedupe on `cursor.event.id`) and unordered. The wire format "may change".

**Hooks.** Local hooks in `~/.cursor/hooks.json` or a project's `.cursor/hooks.json`. They are the only local source.
- Events:
  - `sessionStart` and `sessionEnd`;
  - `beforeSubmitPrompt`;
  - `preToolUse`, `postToolUse` and `afterShellExecution`;
  - `afterFileEdit`;
  - `afterAgentResponse` and `afterAgentThought`;
  - `subagentStart` (with `git_branch`) and `stop`.
- Each payload has `conversation_id`, `generation_id`, `model`, `workspace_roots`, `user_email` and `transcript_path`.
- Hook payloads carry no tokens and no cost.

### 2.6 Others

- **opencode.** It exports OTLP/HTTP logs and traces when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. *Unverified*: span names, whether they are AI SDK spans, and whether content is included.
- **Cline.** It honours `OTEL_TELEMETRY_ENABLED` and the standard `OTEL_*_EXPORTER` variables, with `http/json` as the default protocol. Its names are *unverified*.
- **Kiro.** A daily server-side export of metrics only.
- **Windsurf, Aider and Amp.** No native OTel found (*unverified*).

## 3. How the competitors do it

| | Mechanism | Logs / metrics stored | Agents | Repo, branch, user | PR / per-repo cost | Coding-agent UI |
|---|---|---|---|---|---|---|
| **LangWatch** | Native OTel of all three signals; a CLI (`langwatch claude`, `langwatch instrument <tool>`) writes the agent's settings; a companion hook posts a `langwatch.session_context` log with git context; Codex: `notify` + rollout harvest; gateway optional | yes / yes | Claude Code, Codex, Gemini CLI, Copilot, opencode, Pi, Grok | repo, branch, worktree from the hook and a `langwatch ingest context` command the agent is told to run | **yes**: branch → PR through a GitHub integration; a CI comment with the PR's usage | sessions (cache health, terminal replay), pull requests, usage |
| **Opik** | Claude Code plugin (hooks, transcript); native OTel traces with `ClaudeCodeMappingRules`; paid local proxy (Cost Intelligence) | no / no | Claude Code, Cursor extension, Copilot, Codex partial | proxy: identity, repo, branch, head sha, per-turn git delta | per user and per repo (paid); no PR | Cost Intelligence leaderboard (paid) |
| **MLflow** | Plugin with a `Stop` hook that rebuilds the trace from the transcript at session end; OTel translators for `claude_code.*` and Gemini CLI spans; Codex `notify` | no / no | Claude Code, Codex, Gemini CLI, opencode, Qwen Code | session, OS user, cwd; Codex git metadata | no | none specific |
| **Phoenix** | Hook plugins in an external repo, emitting OpenInference spans | no / no | Claude Code, Codex, Gemini, Cursor, Copilot, opencode, Kiro, … | session | no | none specific |
| **Langfuse** | Rust gateway that recognizes agents by headers (`x-claude-code-session-id`, `x-codex-turn-metadata`, …); OTLP traces only, metrics a no-op, logs rejected | no / no | Claude Code, Codex, opencode, Pi (gateway) | `session.id = <agent>:<id>`; no repo | no | standard sessions |
| **Helicone** | Proxy (`ANTHROPIC_BASE_URL`) | — | Claude Code, Codex | none | no | none |
| **Laminar** | Out-of-repo Claude Code proxy; logs queued, metrics stub | partly / no | Claude Code, opencode parser | none | no | subagent grouping in the trace view |

### 3.1 LangWatch

LangWatch has the most complete design (`dev/docs/adr/056-coding-agent-pipeline-session-aggregate.md`, `modules/coding-agent/contract/src/telemetry/`, `docs/coding-agents/*.mdx`).

**The session is the aggregate.** It folds three kinds of facts, keyed by `session.id` or `gen_ai.conversation.id`:
- span facts;
- log facts (lengths, ids and counters, never content);
- metric facts.

The design rests on one finding: metrics carry no trace context, only `session.id`, and they are the only source of lines of code, commits, PRs and active time.

**Logs without a trace id** are given a synthetic trace id, hashed from the session key.

**Cost** is computed from LangWatch's own price catalog. The agent's own number is kept beside it (`agentReportedCostUsd`).

**PR cost** is a join, not an attribute:
- a hook reports the repository and branch of each session;
- the GitHub integration maps each branch to the PRs whose head is that branch;
- cost per PR is the sum over every session on that branch, from every contributor.

**Their limits.** LangWatch's docs list these:
- A turn that touches two repositories is attributed to the one declared last.
- A `cd` inside the Bash tool does not move attribution unless the agent declares it.
- Work outside a checkout has no PR.

**Their wrapper turns on raw API bodies and tool content by default.** So they had to add a 256 KB-per-attribute guard.

**When spans are off** (no beta flag), cost still arrives through the logs, but subagents fold into the main session.

### 3.2 Hook and transcript integrations

MLflow, Phoenix and the Opik plugin rebuild traces from the transcript JSONL. This gets full content without the OTel opt-ins, but it has costs:
- it is coupled to an internal, unversioned format;
- timing is reconstructed, not measured (MLflow splits tool durations proportionally);
- MLflow exports only at session end, so a killed session loses everything.

### 3.3 Proxies

Helicone, the Langfuse gateway and the Opik paid proxy see exact provider usage. They have three limits:
- they never see tool execution;
- they need an API key or a base-URL override;
- they probably cannot proxy a Claude subscription login.

## 4. What cetra accepts today

| Signal | Endpoint | What happens |
|---|---|---|
| Traces | `POST /v1/traces` (protobuf, JSON), gRPC `TraceService/Export` | Normalized and stored (spec 02). |
| Logs | `POST /v1/logs`, gRPC `LogsService/Export` | Authenticated, size-checked and decoded. GenAI content events are counted in `usage_daily.genai_log_events` (`crates/otel/src/logs.rs`), then **dropped** (D-G-40). |
| Metrics | `POST /v1/metrics`, gRPC `MetricsService/Export` | Authenticated and size-checked, **not decoded**, dropped (D-G-40). |

Every exporter above can reach these endpoints. The auth is `Authorization: Bearer sk-…` through `OTEL_EXPORTER_OTLP_HEADERS`, or Codex's `headers`. Two exceptions:
- **Copilot CLI** refuses a plain `http://` endpoint, so a localhost cetra needs TLS or a proxy.
- **Cursor** needs a public HTTPS endpoint that accepts protobuf, which cetra cloud is.

### 4.1 How today's normalizer reads each agent's spans

This is read from spec 02's tables. It is to be confirmed by captures.

| Agent span | Detected as | What goes wrong |
|---|---|---|
| Claude Code `claude_code.llm_request` | `genai` (has `gen_ai.system`); kind `llm` by the `gen_ai.request.model` fallback | **Tokens are lost.** `input_tokens`, `output_tokens`, `cache_read_tokens` and `cache_creation_tokens` match no source, so tokens are null and cost is `none`. The input count excludes cache, so mapping it needs the D110 exclusive flag. |
| Claude Code `claude_code.tool` | kind `tool` (`gen_ai.tool.call.id` fallback) | The span name is `claude_code.tool` for every tool; the tool is in `tool_name`. Tool input and output exist only as a `tool.output` span event and opt-in attributes, so they never reach `input`/`output`. |
| Claude Code `claude_code.interaction` | kind `span` | It should be the turn's `agent` span, with `user_prompt` as its `text` input. The `<REDACTED>` placeholder must read as uncaptured, like OpenInference's `__REDACTED__` (D-B1-10). |
| Claude Code context | `session.id` → `session_id` ✓; `user.id` → `user_id` ✓ | `user_id` becomes the **anonymous** install id, not the person. `user.email` and `vcs.*` stay in `attributes`. Grouping by person or repository works only through `attributes['…']` in widgets and SQL. |
| Codex `turn` / `completed` / tool spans | `genai` on the `completed` span (it has `gen_ai.usage.*`); kind `span` everywhere (no model attribute, no operation name) | **Tokens are lost by rule 3** (tokens only on `llm`). No model on the usage span, so no cost. Turn tokens use `codex.turn.token_usage.*` names. |
| Gemini CLI `llm_call` / `tool_call` / `agent_call` / `user_prompt` | `genai` | `gen_ai.operation.name` values `llm_call`, `tool_call` and `agent_call` are not in the kind table. `llm_call` falls back to `llm` (it has `gen_ai.request.model`), so usage and messages work. `tool_call` falls back to `tool` only if `gen_ai.tool.name` is set. `agent_call` and `user_prompt` become `span`. `gen_ai.conversation.id` → `session_id` ✓. |
| Copilot `invoke_agent` / `chat` / `execute_tool` | `genai`, kinds `agent` / `llm` / `tool` | Works as is. Rule 3 keeps the root's total usage from double counting. `enduser.pseudo.id` is not a `user_id` source, and the `github.copilot.git.*` attributes stay in `attributes`. |
| Cursor | — | No spans at all. |

So:
- Copilot needs nothing.
- Gemini needs operation names.
- Claude Code needs its token names, the interaction kind and the redaction marker.
- Codex needs a real mapping.
- Cursor and the log-only data need a logs path, which cetra does not have.

### 4.2 What the dashboards can already do

Spec 05's widget breakdowns accept `user_id`, `metadata['k']`, `attributes['k']` (spans) and `resource_attributes['k']` (spans).

So "cost by repository" is possible today. The repository has to be in span attributes or in the resource, and the widget has to use the `spans` source with `cost_total` on `llm` spans. The `traces` source breaks down only by `user_id`, `metadata` and the trace fields. A repository kept in `attributes` therefore cannot drive the traces list, the session list or the users page.

Normalizing the repository, branch and person into first-class or `metadata` fields is what turns these into one-click views.

### 4.3 Capture

`tools/capture` records real instrumentations against a mock provider (D114). Its receiver takes traces only.

The agents can be pointed at a mock:
- Claude Code: `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY`;
- Codex: a `model_providers` entry with a `base_url`;
- Gemini CLI: `GOOGLE_GEMINI_BASE_URL` (*unverified*);
- Copilot: no documented base-URL override (*unverified*).

So captured fixtures for the CLI agents look feasible, with scripted `-p` / `exec` runs. The mock must then speak each agent's API: Anthropic Messages streaming with tool use, and the OpenAI Responses API over SSE or WebSocket for Codex.

## 5. Implications for the design

These are inputs to the design doc, not decisions.

1. **Spans first.** Spans are the one signal every local agent emits and the one cetra stores. Normalizing Claude Code, Codex and Gemini CLI spans is the core of the track, and it is small: new sources in the existing precedence tables, plus a few kind rules.
2. **Content needs the agents' opt-ins.** Prompts, tool I/O and assistant text are off by default in Claude Code, Codex and Copilot.
   - The setup we document chooses what to turn on.
   - Claude Code's assistant text reaches spans only through the allowlisted detailed beta, or through logs (`assistant_response`).
   - The trace view of a Claude Code turn therefore shows the prompt and the tools, not the answer, unless we read logs or a transcript.
3. **Cost.** Claude Code and Codex report their own cost only on logs and metrics. Our price table prices the spans' tokens, since the models are in the LiteLLM snapshot. Gemini reports no cost.
4. **Repository and branch** are not on spans by default. Two ways to get them:
   - set them as resource attributes when the agent starts, through a wrapper, a hook or `OTEL_RESOURCE_ATTRIBUTES` (only Claude Code derives `vcs.*` itself, and only when opted in);
   - send them from a hook, as LangWatch does.
5. **Cost per PR** needs a branch → PR join, which needs a GitHub integration. Cost per branch needs only the branch.
6. **Logs and metrics.** Ingesting them would add:
   - Claude Code's assistant text and reported cost;
   - Codex's content;
   - Cursor entirely;
   - lines of code, commits and PRs.

   That is a new storage path. Logs could be folded into spans, either by synthesizing spans or by attaching them to the matching span by `prompt.id` or `request_id` at query time. Metrics would need a new table. LangWatch is the only competitor that did it, with an event-sourced session aggregate.
7. **A hook package** (`cetra` plugin for Claude Code, hooks for Codex, Gemini and Cursor) could:
   - add cwd, repository and branch;
   - add a real person id;
   - add the assistant's last message;
   - work without the beta flag, by reading the transcript.

   The price is coupling to unversioned payloads and transcripts, and a package to ship and maintain per agent.
