# M7 spike: the cetra pi extension

Status: spike, not a plan. D-IN-27 item 2 says cetra ships its own pi extension that `cetra agents setup pi` installs; this spike reads pi 1.5.2's extension API, proves the GenAI mapping end to end with a throwaway extension, and collects the facts `setup` needs. Nothing here changes the design; § 7 needs the user.

Evidence roots:

- **E** (previous spike): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7` — tools (`tools/netrun.sh`, `tools/server.py`, `tools/h.py`), the norm harness (`norm-target/release/norm`, built from this worktree's `crates/otel`), and the pi install (`pkgs/pi`, `@earendil-works/pi-coding-agent` 1.5.2).
- **P** (this spike): `E/m7-pi` — the prototype extension (`P/ext/cetra-pi.ts`), its runner (`P/run.sh`), and the runs (`P/runs/<name>/`).

Tags: **run** = observed by a command, with the evidence path; **docs** = read from pi's own package (`E/pkgs/pi/node_modules/@earendil-works/...`), LangWatch's repo, or this worktree, with file:line.

## 1. Versions

| What | Value | Evidence |
|---|---|---|
| pi installed and pinned | `@earendil-works/pi-coding-agent` **1.5.2** | **run**: `E/pkgs/pi/node_modules/@earendil-works/pi-coding-agent/package.json` |
| 1.5.2 publish time | 2026-11-10T12:24:55.226Z | **run**: npm packument in `E/npm-cache/_cacache` (integrity sha512 verified against the cache entry; see § 8) |
| 1.5.2 tarball | `https://registry.npmjs.org/@earendil-works/pi-coding-agent/-/pi-coding-agent-1.5.2.tgz`, sha256 `691bde186273b457d9d1c41d0fd47b57853b5e85ded1add1a1f6b78e66c261f4` (7 899 382 bytes; sha1 `26300ab187a9b5a6db0efc8dd4d41a6c6bfcd610` equals the packument's `shasum`) | **run**: sha256 computed over the integrity-verified cached tarball |
| Next release | 1.6.0, published 2026-11-20T11:09:15.529Z — after the 2026-11-17 cutoff, not eligible | **run**: same packument |
| Verdict | 1.5.2 is the newest release published on or before 2026-11-17, and it is the installed version, so "do the event names differ between 1.5.2 and the pinned version" is trivially no | — |
| LangWatch | same package, version line **0.84.2** — their workspace's resolution when ADR-188 was framed (2026-10-26), not a registry pick: the ADR marks the min version **UNVERIFIED** ("the npm registry was not queried") and says a session was proven end to end on 0.73.1 | **docs**: `/home/sebastien/github/langwatch/dev/docs/adr/188-pi-as-a-natively-instrumented-coding-agent.md` lines 15, 57–58, 659–660 |

Event-name differences between LangWatch's 0.84.2 and 1.5.2 (read from their ADR's type citations vs the 1.5.2 types): the core event names they use are unchanged (`session_start`, `session_shutdown`, `agent_start`, `agent_end`, `agent_settled`, `turn_start`, `turn_end`, `message_end`, `tool_execution_start`, `tool_execution_end`, `before_provider_request`, `after_provider_response`); two payloads moved — `provider_stream_event` was not in the 0.84.2 event union (ADR line 109) but is at 1.5.2 (`types.d.ts:733`), and `TurnEndEvent` was `{type, turnIndex, message, toolResults}` (ADR line 106) while 1.5.2 adds `BoundaryState` (`entries`, `continue`, `context`, `outcome`) (`types.d.ts:781–786`, `848`).

## 2. Method

- Every pi run executes inside `unshare -Urn` (loopback only) through `E/tools/netrun.sh`, against the fake provider and OTLP sink of `E/tools/server.py` (one tool-call turn at 82/17 tokens, then a text turn at 131/12; fake key `sk-fake-m7`). **run**: `P/runs/*/run.log`, `trace.strace`.
- The prototype is `P/ext/cetra-pi.ts` (170 lines with comments, zero imports — only globals), installed by `P/run.sh` into the sandbox home's `~/.pi/agent/extensions/`. `P/run.sh` exports `OTEL_EXPORTER_OTLP_ENDPOINT=<sink>`, `OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer%20sk-fake-m7'`, `OTEL_SERVICE_NAME=pi`; per-run knobs come from the run's `env` file (`PI_CONTENT`, `PI_FLUSH`, `FAKE_CWD`).
- Runs: `P/runs/pi-ext-content` (content on), `P/runs/pi-ext-nocontent` (content off), `P/runs/pi-ext-shutdown` (flush only at `session_shutdown`), `P/runs/pi-ext-broken` (unparseable extension). Each normalizes with cetra's real normalizer via `E/tools/h.py` into `P/runs/<name>/out/<name>.norm.jsonl`.

## 3. pi's extension API at 1.5.2

### Loading and shape

- Extensions are TypeScript or JavaScript files that pi **auto-discovers** from `~/.pi/agent/extensions/*.ts|*.js` (one level deep; subdirectories need `index.ts` or a `package.json` with a `pi.extensions` field), project-local `.pi/extensions/` after trust, and extra paths listed in `settings.json` (`packages`, `extensions`). No config entry is needed for a plain file in the folder. **docs**: `E/pkgs/pi/node_modules/@earendil-works/pi-coding-agent/docs/extensions.md:127–151`. **run**: `P/run.sh` only copies the file, and pi loaded it (`P/runs/pi-ext-content/out/pi-ext-content.dump.txt`, scope `cetra-pi-extension@0.1.0`).
- The file default-exports a factory `function (pi: ExtensionAPI)`; sync or async (an async factory is awaited before `session_start`). Handlers register with `pi.on(event, handler)`. Loading goes through jiti, so TypeScript needs no build step; npm dependencies resolve from a `node_modules` beside the file (the prototype has none). **docs**: extensions.md:186–211, 215–224.
- Factory-time rules: factories may run in invocations that never start a session, so background resources must start at `session_start` or later, and `session_shutdown` must be idempotent. **docs**: extensions.md:252–256.
- An extension that fails to parse is **fatal at startup**: pi prints `Error: Failed to load extension ...` and exits 1 before any model call; `pi -ne` skips extensions. **run**: `P/runs/pi-ext-broken/run.log` (a `1 || 2 ?? 3` line, a plain JS syntax error). **docs**: `E/pkgs/pi/node_modules/@earendil-works/pi-coding-agent/dist/main.js:50` (`EXTENSION_LOAD_FAILURE_HINT`). Note jiti accepts `??`, `?.` and TypeScript annotations (all used in the working prototype); only genuine syntax errors kill it.
- The config dir honours `$PI_CODING_AGENT_DIR` (else `~/.pi/agent`), matching D-IN-27's path. **docs**: `dist/config.js:495` (`ENV_AGENT_DIR = PI_CODING_AGENT_DIR`), `:510–515` (`getAgentDir`), `docs/environment-variables.md:81`.

### Events and payloads

All types in `E/pkgs/pi/node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/types.d.ts` (below, "types.d.ts"); message types in `E/pkgs/pi/node_modules/@earendil-works/pi-ai/dist/types.d.ts` ("pi-ai"). Docs: extensions.md § Events (lines 307–1160).

| Purpose | Event | Payload fields | Where |
|---|---|---|---|
| Session start | `session_start` | `reason: "startup"\|"reload"\|"new"\|"resume"\|"fork"`, `previousSessionFile?`; the id comes from `ctx.sessionManager.getSessionId()` (a UUID also in ephemeral `-p` sessions) | types.d.ts:595–600; session-manager.d.ts:253 (`getSessionId()`), `:696` (`sessionId = createSessionId()`) |
| Session end | `session_shutdown` | `reason: "quit"\|"reload"\|"new"\|"resume"\|"fork"`, `targetSessionFile?` | types.d.ts:657–662; extensions.md:601 |
| Agent (run) start | `agent_start` | no fields | types.d.ts:764–766 |
| Agent (run) end | `agent_end` | `messages: AgentMessage[]` (this run's messages) | types.d.ts:768–771 |
| Run fully settled | `agent_settled` | `aborted: boolean` | types.d.ts:821–824 |
| Turn start | `turn_start` | `turnIndex: number`, `timestamp: number` | types.d.ts:842–846 |
| Turn end | `turn_end` | `turnIndex`, `message: AgentMessage` (the assistant message), `toolResults: ToolResultMessage[]`, `messageEntryId`, `toolResultEntryIds`, plus `BoundaryState` (`entries`, `continue`, `context`, `outcome: "completed"\|"aborted"\|"error"`) | types.d.ts:781–786, 848–856 |
| Model request (context) | `context`, `context_with_system` | `messages: AgentMessage[]` — without / with the system message; fire before each LLM call | types.d.ts:699–711; extensions.md:815–845 |
| Model request (payload) | `before_provider_request` | `payload: unknown` — the provider-serialized request (for the openai-completions API: `{model, messages, tools, stream}`); can be replaced | types.d.ts:713–716; extensions.md:864–881; **run**: the equivalent HTTP bodies in `P/runs/pi-ext-content/req/` |
| Model response (HTTP) | `after_provider_response` | `status: number`, `headers` | types.d.ts:727–731 |
| Model response (stream) | `provider_stream_event` | `provider`, `api`, `model`, `data` (parsed stream event) | types.d.ts:733–738 |
| Assistant message | `message_start` / `message_update` / `message_end` | `message: AgentMessage`; `message_end` can replace it | types.d.ts:857–871 |
| Tool execution | `tool_execution_start` / `_update` / `_end` | start: `toolCallId`, `toolName`, `args`, `parentToolCallId?`; end: `toolCallId`, `toolName`, `result`, `isError`, `durationMs?` (absent when the tool did not run), `parentToolCallId?` | types.d.ts:873–902 |
| Tool call / result (gate/transform) | `tool_call`, `tool_result` | `tool_call`: `toolName`, `toolCallId`, mutable `input`; `tool_result`: adds `content`, `details`, `structuredContent`, `isError`, `usage` | types.d.ts:962–1059 |
| Model change | `model_select` | `model`, `previousModel?`, `source` | types.d.ts:905–910 |

The assistant message is the single source for the model-call facts (**docs**, pi-ai types.d.ts): `AssistantMessage` (`:382`) carries `api`, `provider`, `model`, `responseModel?`, `content: (TextContent | ThinkingContent | ToolCall)[]`, `usage: Usage`, `stopReason: StopReason`, `timestamp`, `durationMs?`. `Usage` (`:292–319`) is `{input, output, cacheRead, cacheWrite, cacheWrite1h?, reasoning?, totalTokens, cost{input, output, cacheRead, cacheWrite, total, totalFromProvider?}}`; `reasoning` is documented as a **subset of `output`** — the same semantics cetra's GenAI token table assumes. `StopReason` (`:321`) is `"pending" | "stop" | "length" | "toolUse" | "error" | "aborted" | "deferred"`. `ToolCall` (`:278`) is `{type: "toolCall", id, name, arguments: JsonObject}`; `ToolResultMessage` (`:434`) is `{role: "toolResult", toolCallId, toolName, content: (TextContent|ImageContent)[], isError, ...}`. The tool result of `tool_execution_end` is the tool's execute() return (`content` parts array). LangWatch's ADR reached the same conclusions at 0.84.2 (lines 111–120).

pi has **no telemetry backend**: zero `OTEL_`/`otlp`/`opentelemetry` references in `pi-coding-agent`'s dist (**run**: grep over `dist/**/*.js`), `@earendil-works/pi-telemetry`'s README says "no exporter, global current-span state, or dependency on a telemetry backend" (`E/pkgs/pi/node_modules/@earendil-works/pi-telemetry/README.md:13`), and settings.json has no env or telemetry-export key (**run**: grep of `dist/core/settings-schema.d.ts`). `PI_TELEMETRY` is only the install-analytics opt-out (`dist/core/telemetry.js:6`).

## 4. The prototype, and what the normalizer did with it

`P/ext/cetra-pi.ts` maps pi events to GenAI semconv spans and posts them as OTLP/HTTP **JSON** with one `fetch` per flush (no OTel SDK, no dependencies):

- `invoke_agent` — opened at `agent_start`, closed at `agent_end` (or `session_shutdown` if the run never ended); attributes `gen_ai.operation.name`, `gen_ai.conversation.id`.
- `chat <model>` — one per assistant message (`message_end`), started at the preceding `before_provider_request`; attributes `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, the six `gen_ai.usage.*` keys (input, output, cache read/write, total, reasoning when defined), `gen_ai.response.finish_reasons` (array; pi's `toolUse` mapped to `tool_calls`), `gen_ai.conversation.id`; with content capture, `gen_ai.input.messages` (from `context_with_system`, converted pi → OpenAI chat format) and `gen_ai.output.messages` (the assistant message, same converter).
- `execute_tool <name>` — one per `tool_execution_start`/`_end`; attributes `gen_ai.operation.name`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.conversation.id`, and with content capture `gen_ai.tool.call.arguments` / `gen_ai.tool.call.result`; OTLP status error when `isError`.
- All spans share one trace id per process; chat and tool spans parent the `invoke_agent` span id. Export honours `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` else `OTEL_EXPORTER_OTLP_ENDPOINT` (+ `/v1/traces`), `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`, and is inert when no endpoint is set.

**Normalized result** (`P/runs/pi-ext-content/out/pi-ext-content.norm.jsonl`, content on; **run**; 4 spans kept, 0 rejections, 0 discarded, conventions `["genai"]` only — no Pi-specific rule):

| Span | kind | provider | model | tokens in/out | cost | I/O |
|---|---|---|---|---|---|---|
| `invoke_agent` | agent | — | — | — | — | — |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini | 82 / 17 | 2.25e-05 (computed) | input messages (system, user); output: one assistant message with a `read` tool call, finish reason `tool_calls` |
| `execute_tool read` | tool | — | — | — | — | input: `{"path": "…/hello.txt"}` (json); output: the file text |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini | 131 / 12 | 2.685e-05 (computed) | input: full conversation incl. the assistant tool call and the tool result; output: the answer text, finish reason `stop` |

`session_id` (from `gen_ai.conversation.id`) is set on all four rows; parent ids are consistent (dump `parent=#0`). With content off (`P/runs/pi-ext-nocontent`): identical kinds, tokens, provider, model and cost, and **no** payload attributes (**run**: `out/pi-ext-nocontent.norm.jsonl`).

## 5. The three behaviour questions

- **Does `fetch` at exit lose spans?** No. With `PI_FLUSH=shutdown` the extension flushed **only** from the `session_shutdown` handler, and all 4 spans arrived in one body: a `pi -p` process does not exit before that handler's `await fetch` completes (whether pi awaits the handler or the in-flight socket keeps Node's loop alive is indistinguishable from outside, and irrelevant to the result). **run**: `P/runs/pi-ext-shutdown/` (1 OTLP body, 4 spans in `recv/0001-traces.json`). The flush is bounded by `AbortSignal.timeout(5000)` so a dead endpoint cannot hold pi's exit — the same concern as LangWatch ADR-188 Decision 9 (lines 504–508). The default variant also flushes at `agent_end`/`agent_settled` so interactive sessions export per run.
- **`OTEL_EXPORTER_OTLP_HEADERS` percent-encoding.** The OTel convention is comma-separated `key=value` pairs with percent-encoded values. The run set `Authorization=Bearer%20sk-fake-m7`; the sink received the **decoded** `Authorization: Bearer sk-fake-m7` (17 chars). **run**: `P/runs/pi-ext-content/http.jsonl`. Decoding must be: split each pair at the first `=`, trim, then percent-decode the value — `decodeURIComponent` with a raw fallback for malformed sequences (it throws on a lone `%`). `+` is not a space (this is percent-encoding, not form encoding), and a literal comma inside a value must be written `%2C`. `cetra agents setup` must therefore write the **encoded** form, as it does for the other `OTEL_*` agents.
- **Should content capture be opt-in?** Yes, and the prototype gates it on `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` — the same flag Goose and Oh My Pi read (D-CA-10 keeps content off by default). Without the flag, prompts, completions, tool arguments and results never leave the machine; tokens, models and costs still do. **run**: the nocontent run above.

## 6. Event-to-span mapping table

The mapping D-IN-27 item 2 describes, as proven in § 4:

| pi event(s) | Span emitted | Timing | GenAI attributes |
|---|---|---|---|
| `agent_start` … `agent_end` (fallback `session_shutdown`) | `invoke_agent` (root, per run) | start at `agent_start`, end at `agent_end` | `gen_ai.operation.name=invoke_agent`, `gen_ai.conversation.id` |
| `before_provider_request` + `context_with_system` + `message_end` (assistant) | `chat <model>` (child of `invoke_agent`, one per model call) | start at `before_provider_request`, end at `message_end` | `gen_ai.operation.name=chat`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.{input,output,cache_read.input,cache_write.input,reasoning.output,total}_tokens`, `gen_ai.response.finish_reasons`, `gen_ai.conversation.id`; with content: `gen_ai.input.messages`, `gen_ai.output.messages` |
| `tool_execution_start` … `tool_execution_end` | `execute_tool <name>` (child of `invoke_agent`) | start/end at the events | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.conversation.id`; with content: `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`; OTLP status error on `isError` |
| `ctx.sessionManager.getSessionId()` | — | read at `session_start`/`agent_start` | `gen_ai.conversation.id` on every span |

Every key above is one cetra's normalizer already reads generically (spec 02 § Mapping tables; `crates/otel/src/conventions/genai.rs:20–31`, `usage.rs:64–272`, `context.rs:132`), which is why the normalized output needed no Pi rule.

## 7. What `cetra agents setup pi` would write (facts; no pick)

- **File**: one TypeScript file in pi's global extension folder — `$PI_CODING_AGENT_DIR/extensions/<name>.ts`, default `~/.pi/agent/extensions/<name>.ts` (**docs**: `dist/config.js:495,510–515`; extensions.md:131). Auto-loaded, no config entry (**run**: § 3). D-IN-27 already decides the marker line (first-line comment naming cetra, LangWatch's `PI_EXTENSION_MARKER` precedent at `pi-extension-source.ts:12`) so setup and `remove` never touch a file they did not write.
- **Endpoint and key**: pi has **no config file** for them (§ 3: no `OTEL_*` reader, no settings env block), so they must reach pi's process environment. The two options, as facts:
  1. **A shell function / wrapper command** (LangWatch's route: their `langwatch pi` wrapper injects the `OTEL_*` env and re-ensures the extension file on every launch — ADR-188 Decision 4, lines 231–249). The env is present only when pi is launched through the wrapper; the wrapper must be on `PATH` or aliased; it also guarantees file freshness per launch. Without the env the extension is inert (no endpoint set → no export), so a bare `pi` still works, untraced.
  2. **The user exports the variables** (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` with the percent-encoded key, optionally `OTEL_SERVICE_NAME` and the content flag) in their shell profile. Works for every launch path (terminal, IDE integration, scripts, `pi install`-launched packages); the key sits in the environment of all child processes; `cetra agents setup pi --print` (D-CA-10's print mode) can emit the exact lines; nothing re-ensures the extension file after upgrades.
- Operational notes for the plan: a broken extension file is fatal to pi (§ 3), so the shipped file must be the one tested against the pinned pi and setup should refuse to overwrite a foreign file (the marker); users can opt out per run with `pi -ne` or by deleting the file.

## 8. Open questions (need the user or the M7 plan)

1. **System prompt**: `context_with_system`'s leading system message arrives with `content: ""` — pi's prompt text rides in the message's `sections`, which the prototype drops, so captured input lacks the system prompt (**run**: § 4 input messages). Reading it as the model saw it means taking the serialized prompt from `before_provider_request`'s payload, which is provider-specific (`openai-completions` vs `anthropic` vs …). Accept prompt-less input, or serialize per API?
2. **Trace shape**: the prototype mints one trace per process, with one `invoke_agent` per prompt inside it (OMP's shape). Alternative: one trace per `agent_start`. Which does the dashboard want for interactive sessions?
3. **Retries**: `before_provider_request` fires per attempt and `message_end` per assistant message, so a provider retry that produces a new assistant message yields two `chat` spans (each with its own usage). Untested (the fake never retries). Confirm or accept.
4. **Compaction and steering model calls**: a compaction summary request (`session_before_compact`) is also a model call whose assistant message flows through `message_end`, so it would appear as a `chat` span under `invoke_agent`. LangWatch emits a separate `pi.harness.compaction` span (ADR lines 334–345). Fold compaction calls into `chat`, or mark them?
5. **Custom messages**: extension-injected custom messages (CustomAgentMessages) are skipped by the prototype's converter. Fine for built-in tools; confirm if sessions using such extensions must still convert.
6. **Naming**: the extension file name (e.g. `cetra.ts` vs `cetra-pi.ts`) and the exact marker line — plan decision.
7. **Interactive TUI path**: only `pi -p` was run (no TTY in the sandbox). `session_shutdown` reason `quit` and per-run flushing in a long interactive session are untested.

## 9. Network

**This session: nothing left loopback.** All four runs went through `unshare -Urn`; every `connect()` in their straces targets `127.0.0.1` (2 per run: the keep-alive provider connection and the OTLP fetch) — **run**: `P/runs/*/trace.strace`. No `npm`, no `WebFetch`, no other download was run by this session.

The version facts in § 1 were read **offline** from the previous agent's npm cache (`E/npm-cache/_cacache`): the packument for `@earendil-works/pi-coding-agent` and the 1.5.2 tarball, both integrity-verified (sha512) against their cache entries before use. Those bytes were originally downloaded from `registry.npmjs.org` by the previous agent (recorded in `spikes/m7-coding-agents.md` § 8); this spike adds no new download. Recorded for the pinned version: URL `https://registry.npmjs.org/@earendil-works/pi-coding-agent/-/pi-coding-agent-1.5.2.tgz`, published 2026-11-10T12:24:55.226Z, sha256 `691bde186273b457d9d1c41d0fd47b57853b5e85ded1add1a1f6b78e66c261f4`.
