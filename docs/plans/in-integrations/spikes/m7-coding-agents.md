# M7 spike: coding agents

Status: spike, not a plan. Continues a spike that a previous agent started and could not finish. Nothing here changes the design or the decisions; the open questions in § 7 need the user.

Evidence root (E): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7`. Paths below are relative to E unless absolute. Each fact is tagged:

- **run**: observed by a command (the previous agent or this session), with the evidence path.
- **docs**: read from a package's own docs or source in E (`pkgs/`), or from a repo doc, with the path.

## 1. Scope and what changed in this session

- Items: OpenCode, Qwen Code, Hermes Agent, Goose, OpenClaw, Augment Code (`auggie`), Oh My Pi, Omnigent, Pi, Devin SDK.
- The previous agent's last note was "my sink did not log OTLP request headers, so that check proved nothing". **run**: `tools/server.py` was changed at 2026-11-24 02:57:26 (file mtime). The runs before that time (`qwen-hdr`, 02:57:15, and everything else) show empty header lists in their `otlp` events. Those runs say nothing about auth. The runs after it (`qwen-otelhdr-hdr` 02:57:31, `qwen-otelhdr-nohdr` 02:57:28, `qwen-dotenv` 02:57:46) log headers and are valid. That is the "both cases" re-test the note asked for, and it is done. **run**: `runs/qwen-otelhdr-*/http.jsonl`, `runs/qwen-dotenv/http.jsonl`.
- Header re-runs in this session, on the fixed sink, in fresh run directories: `runs/hdr-omp`, `runs/hdr-goose`, `runs/hdr-oc`, `runs/hdr-hermes`. Each sent its OTLP requests to loopback only (§ 8).
- Not run: Augment Code (stopped: its CLI needs an Augment login, and I found no base-URL override to point it at loopback), Devin SDK (an app-embedded library with no agent backend in the sandbox), Omnigent (not identifiable, see § 7). OpenClaw's gateway path failed before export (see § 4).

## 2. Method

- Each run executes inside `unshare -Urn` (user and network namespace with loopback only). **run**: `tools/netrun.sh`, `tools/inner.sh`. The fake provider and the OTLP sink (`tools/server.py`) run on 127.0.0.1 inside the namespace. Inside a run, `strace` records `connect()` calls (`trace.strace`).
- The sink logs each request's method, path, content type, content encoding, transfer encoding and redacted headers (`http.jsonl`), and stores the raw body (`recv/`). Gzip and chunked bodies are decoded by the sink. **run**: `tools/server.py`.
- Normalization uses cetra's real `normalize` from the worktree's `crates/otel`, via the harness `norm/` (release binary `norm-target/release/norm`). Output: `runs/<run>/norm.jsonl`. **run**: `norm/src/main.rs`.
- Runners: `runners/*.sh`. Fake keys only (`sk-fake-m7`).

## 3. Verdict table

Verdicts use the design's four labels. "Proposed" means a recommendation for the user to decide; it is not a decision.

| Item | Version (installed) | Published (UTC) | Telemetry route | Verdict (proposed) | Main reason, with evidence |
|---|---|---|---|---|---|
| OpenCode | npm `opencode-ai` 1.19.6; separate v2 binary 2.3.0 (`dl/`) | 1.19.6: 2026-11-17T07:22:20Z | Env vars: `OPENCODE_ENABLE_TELEMETRY`, `OPENCODE_OTLP_ENDPOINT`, `OPENCODE_OTLP_PROTOCOL`, `OPENCODE_OTLP_HEADERS` (v2 runner) | **First-class for the v2 build only**; 1.19.6 not supported as tested | v2: LLM, tool and session spans with GenAI-style usage. 1.19.6: SQL noise only, zero model requests. Open: which build ships. |
| Qwen Code | `@qwen-code/qwen-code` 0.28.0 | 2026-11-17T12:10:30Z | `~/.qwen/settings.json` `telemetry` block (OTLP HTTP/JSON) | **First-class** (D-IN-12 already) | Spans carry `gen_ai.*` attributes and normalize with tokens and I/O. Auth sent as documented. Logs and metrics also sent (no per-signal switch, § 7). |
| Hermes Agent | hermes-agent v0.23.2 (NousResearch tag); plugin hermes-otel 1.33.0 (community) | v0.23.2 commit: 2026-11-16T19:28Z; hermes-otel release commit: 2026-11-11T19:50Z | Plugin config (`hermes_otel.yaml`) plus a plugin install | **Captured page** (not first-class) | Spans arrive and normalize, but the telemetry belongs to a third-party plugin the user must install; `agents setup` writes settings only (D-CA-10). Recommendation, not decided. |
| Goose | 1.60.0 (`goose --version`) | not verified | Env vars: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` | **First-class candidate**, with a double-count rule | Spans `reply`, `chat`, `stream_response_from_provider`, `execute_tool`. `stream_response_from_provider` duplicates `chat`'s usage (§ 5). Config-file route did not export. |
| OpenClaw | `openclaw` 2026.11.5 (+ `@openclaw/diagnostics-otel` 2026.11.5 tarball) | openclaw: 2026-11-17T09:59:33Z; plugin: not verified | `openclaw.json` `diagnostics.otel` block | **Not supported** as tested | 0 OTLP bodies in 8 runs. Plugin logged "internal diagnostics capability unavailable" (`runs/openclaw-6/stderr.txt`). Gateway path never ran past auth or CLI flags. |
| Augment Code (`auggie`) | `@augmentcode/auggie` 1.2.2 | 2026-11-17T04:46:01Z | Bundled `otel` extension; env and settings.json | **Undecided** (stopped at login) | Static only (§ 4). Would be a GenAI-attribute source; needs a login-free run or a decision on identity attributes. |
| Oh My Pi | `@oh-my-pi/pi-coding-agent` 18.22.1 | 2026-11-17T21:32:54Z (newer 19.1.1 is 2026-11-23, not eligible) | Env vars: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`; content via `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` | **First-class candidate** | Clean GenAI spans (`chat`, `execute_tool`, `invoke_agent`), no noise, content on with the flag. Header decoded correctly (§ 4). Open: whether the env route is the only one. |
| Omnigent | unknown | unknown | unknown | **Not assessed** | Not installed, no package, no docs in E. Cannot identify the item (§ 7). |
| Pi | `@earendil-works/pi-coding-agent` 1.5.2 | 2026-11-10T12:24:55Z | Extension copied into `~/.pi/agent/extensions` (runner `runners/pi.sh`) | **Setup-only page** | Works only with the extension. Its spans use custom `llm.usage.*` keys the normalizer does not read: kind `span`, no tokens, no provider. |
| Devin SDK | `@cognition-ai/sdk` 0.0.1-beta.9 | 2026-11-12T07:57:55Z | App-owned OpenTelemetry tracer (`traceSessions`), no CLI flag | **Setup-only page** (SDK snippet; no `agents setup` target) | Library, not an agent CLI; the app owns the exporter (docs). Not run: needs an agent backend. Prompts recorded by default (docs). |

Dates are from the npm packuments in the local cache (**run**: `npm-cache/_cacache`, read with `python`, `time` field). The packument for each version is the registry's own publish time.

## 4. Per-item findings

### OpenCode

- Versions: npm `opencode-ai` 1.19.6 (installed, `pkgs/opencode`), published 2026-11-17T07:22:20Z (**run**, cache). A separate v2 binary, printing `opencode v2.3.0` (**run**: `runs/opencode-v2/stdout.txt`), lives in `dl/opencode-v2/` with its tarball `dl/opencode-v2/opencode-v2.3.0.tar.gz`. Its source URL and release date are **not recorded** (§ 8).
- 1.19.6, env-only (`runners/opencode-v1-builtin.sh`): one OTLP body (`/v1/traces`, JSON, chunked), 8 spans in scope `opencode`: 7 `sql.execute`, 1 `SQLiteDrizzle.make`. Zero requests reached the fake provider (`req=0`). **run**: `runs/opencode-v1-builtin*/`. The AI SDK path the design expects (`experimental.openTelemetry`-style flag) was not exercised; `grep` of `pkgs/opencode` finds no OTel flag (**docs**: `pkgs/opencode/node_modules/opencode-ai`, the package is a launcher). Unverified.
- v2 binary (`runners/opencode-v2.sh`): OTLP protobuf to `/v1/traces`, `/v1/logs`, `/v1/metrics`. Spans, service `opencode`, scope `com.opencode`: `opencode.session` (agent), `opencode.llm` x2 (provider `openai`, tokens 82/17 and 131/12, cost reported; the first has no output because it is the tool-call turn), `opencode.tool.read` (tool, I/O present). Model calls went to `/v1/responses`. **run**: `runs/opencode-v2/norm.jsonl`, `runs/opencode-v2/http.jsonl`.
- Header: the value is sent literally as `Bearer%20sk-fake-m7` (19 characters), not percent-decoded. **run**: `runs/hdr-oc/http.jsonl`. Goose and Oh My Pi decode the same encoding correctly (§ 4, Goose and Oh My Pi sections). This matters for setup: the header value cannot simply be copied from the key.
- Caveat: the v2 runner also loads `@devtheops/opencode-plugin-otel` (`runners/opencode-v2.sh`, `pkgs/ocplugin-bundle`). This run does not separate the plugin's spans from the binary's own. **run**: `runners/opencode-v2.sh`.

### Qwen Code

- `@qwen-code/qwen-code` 0.28.0, published 2026-11-17T12:10:30Z (**run**, cache).
- Config used by `runners/qwen.sh`: `telemetry` with `enabled`, `target`, `otlpEndpoint`, `otlpProtocol: http`, `logPrompts`, `includeSensitiveSpanAttributes`. **docs**: `pkgs/qwen/node_modules/@qwen-code/qwen-code/bundled/qc-helper/docs/configuration/settings.md` lines 754–762: `otlpProtocol` `http` is HTTP/JSON (`application/json`); there is one `otlpEndpoint` and no per-signal switch.
- Transport: `application/json`, chunked, to `/v1/traces`, `/v1/logs`, `/v1/metrics`. **run**: `runs/qwen-final/http.jsonl`.
- Auth (valid post-fix runs): with `OTEL_EXPORTER_OTLP_HEADERS` set, `Authorization` is sent (`Bearer sk-fake...`, 17 characters, decoded); without it, no header; with `~/.qwen/.env` holding the variable, it is sent. **run**: `runs/qwen-otelhdr-hdr`, `runs/qwen-otelhdr-nohdr`, `runs/qwen-dotenv`. Header key as sent: `Authorization` (capitalised).
- Spans, service `qwen-code`: `qwen-code.interaction` (agent, I/O), `qwen-code.llm_request` x3 (llm, provider `openai` from `gen_ai.provider.name`, tokens, I/O), `qwen-code.tool` (tool, `execute_tool`, no I/O in this run). Noise: `qwen-code.hook` (only because the spike configured hooks), and a `GET` span from `@opentelemetry/instrumentation-undici` (an outbound fetch; its cause is not verified). **run**: `runs/qwen-final/norm.jsonl`.
- Attributes are GenAI-semconv (`gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.conversation.id`). **run**: same file.

### Hermes Agent

- Agent: NousResearch hermes-agent, git tag `v0.23.2` in `src/hermes-agent` (remote `github.com/NousResearch/hermes-agent`). Commit date 2026-11-16T19:28:45-08:00 (**run**: `git log` on the clone). The npm packument `hermes-agent` in the cache is a different package (0.23.2 published 2026-11-17T03:58:23Z, **run**), not used here.
- Telemetry plugin: `hermes-otel` 1.33.0 by briancaffey (community), tag `hermes-otel-v1.33.0` in `src/hermes-otel`; release commit 2026-11-11T14:50:22-05:00 (**run**: `git log`). Installed in `pkgs/hermes-venv-314` (`hermes_otel-1.33.0.dist-info`, **run**).
- Config (`runners/hermes.sh`): `config.yaml` enables plugin `hermes_otel`; `hermes_otel.yaml` sets an OTLP backend at `$BASE/v1/traces` with `Authorization`. Content: full. **run**.
- Run `hermes-3` and header re-run `hdr-hermes`: OTLP protobuf to `/v1/traces` (2 bodies), `/v1/logs` (19), `/v1/metrics` (6). `Authorization` decoded (`Bearer sk-fake...`, 17 characters). **run**: `runs/hdr-hermes/http.jsonl`. The sink saw `POST /api/show` x4 (an Ollama probe from the custom provider) plus `/v1/chat/completions` x2 (**run**). Those are HTTP calls, not spans.
- Spans, scope `hermes-otel-plugin`, service `hermes-agent`: `agent` (agent), `api.gpt-4o-mini` x2 (llm, tokens, I/O), `llm.gpt-4o-mini` x1 (llm with I/O but no tokens: a wrapper that duplicates an `api.` call), `tool.read_file` (tool, I/O). Conventions: GenAI, OpenInference, OpenLLMetry. **run**: `runs/hermes-3/norm.jsonl`.
- Process exit code 137 in the wrapper after the agent printed its summary (timeout kill). **run**: `runs/hermes-3/stderr.txt`. The agent itself exited 0 (`hermes exit=0` in stdout).

### Goose

- Version `goose --version` = 1.60.0 (**run**: `runs/goose-3/stdout.txt`). Binary in `dl/goose/` (`goose.tar.gz` 60.6 MB). Release date and source URL **not verified** (§ 8).
- Env route (`runners/goose.sh`): `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-fake-m7`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`. Exports protobuf to `/v1/traces`, `/v1/metrics`, `/v1/logs` (**run**: `runs/goose-3/http.jsonl`).
- Header re-run `hdr-goose`: `authorization` sent, decoded as `Bearer sk-fake...` (17 characters). **run**: `runs/hdr-goose/http.jsonl`. Note the lowercase key name as the client sent it.
- Config-file route (`runners/goose-config.sh`, OTEL keys in `config.yaml`): zero OTLP bodies in `runs/goose-config`. **run**. So only the environment route is evidenced.
- Spans, service `goose`: `reply` (agent, parent of all), `chat` x2 (llm, parent `reply`, tokens 82/17 and 131/12), `stream_response_from_provider` x2 (llm, child of `chat`, **same tokens** as the parent), `execute_tool` (tool, I/O). **run**: `runs/goose-3/norm.jsonl` with parent links.

### OpenClaw

- `openclaw` 2026.11.5, published 2026-11-17T09:59:33Z (**run**, cache). Diagnostics plugin `@openclaw/diagnostics-otel` 2026.11.5 from `tarballs/openclaw-diagnostics-otel-2026.11.5.tgz` (release date not verified).
- Runs `openclaw-1` to `openclaw-8`: model requests reached the fake provider (`req=2` in most), **zero** OTLP bodies in every run. **run**: `runs/openclaw-*/`.
- Cause, as the plugin reports it: `[plugins] diagnostics-otel: internal diagnostics capability unavailable` (**run**: `runs/openclaw-6/stderr.txt`). `openclaw-6` completed one agent turn with one tool-call failure; no spans were exported.
- Gateway path (`runners/openclaw-gw.sh`): `openclaw-7` failed with "gateway agent requires credentials"; `openclaw-8` failed with "does not recognize option --password" (**run**: `runs/openclaw-7/stdout.txt`, `runs/openclaw-8/stdout.txt`). Never got past the CLI. Unverified.

### Augment Code (`auggie`)

- `@augmentcode/auggie` 1.2.2, published 2026-11-17T04:46:01Z (**run**, cache). Not run.
- Static, **docs** (bundled code, `pkgs/auggie/node_modules/@augmentcode/auggie/auggie-v2.mjs.d/extensions/otel/index.js`): the extension starts only when telemetry is enabled by `AUGGIE_ENABLE_TELEMETRY` (or `CLAUDE_CODE_ENABLE_TELEMETRY`, or a `telemetry.enabled` setting), and an OTLP endpoint is set (`OTEL_EXPORTER_OTLP_ENDPOINT` or a per-signal one). It reads a `settings.json` in its agent directory: `telemetry` and an `env` block. It emits `gen_ai.*` attributes; message content is off unless `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` is set; `OTEL_LOG_USER_PROMPTS` is a separate switch. The resource carries `AUGGIE_USER_EMAIL`/`AUGMENT_USER_EMAIL` as user identity when set. Path suffixes `/v1/traces`, `/v1/logs`, `/v1/metrics`.
- Stopped: its CLI requires a credentials event (`augment:credentials`) from its own backend. No base-URL override found by search.

### Oh My Pi

- `@oh-my-pi/pi-coding-agent` 18.22.1, published 2026-11-17T21:32:54Z (**run**, cache). Newer 19.1.1 on 2026-11-23 is not eligible.
- Env route (`runners/omp.sh`, `runners/omp-content.sh`). **docs**: the source (`pkgs/omp/node_modules/@oh-my-pi/pi-coding-agent/src/main.ts:2432`, `src/telemetry-export-otlp.ts:165`) names `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` and `OTEL_SERVICE_NAME`.
- Transport: protobuf to `/v1/traces`, `/v1/logs`, `/v1/metrics`. Header re-run `hdr-omp`: `Authorization` decoded (`Bearer sk-fake...`, 17 characters). **run**: `runs/hdr-omp/http.jsonl`.
- Spans, service `omp`, scope `@oh-my-pi/pi-agent-core`: `invoke_agent` (agent), `chat gpt-4o-mini` x5 (llm, tokens), `execute_tool read` (tool). Five model requests reached the fake (`model_posts=5`), so the five `chat` spans are real calls, not duplicates. **run**: `runs/omp-1/norm.jsonl`, `runs/omp-1/http.jsonl`.
- Without content capture (`omp-1`), llm and tool spans have no I/O. With `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (`omp-content`), I/O is present on all llm and tool spans. **run**.
- `provider` is `fake`, taken from the config's provider name `fake/gpt-4o-mini`, not from the endpoint. Expected with the spike's config. **run**.
- Isolation note: `omp` read `/home/sebastien/.claude-extra/skills/...` through the real home (reads only, no writes). **run**: `runs/omp-1/real-home.txt`.

### Pi

- `@earendil-works/pi-coding-agent` 1.5.2, published 2026-11-10T12:24:55Z (**run**, cache). Newer 1.6.0 (2026-11-20) is not used.
- Extension: `pkgs/pi-ext/index.ts`, copied into `~/.pi/agent/extensions` by `runners/pi.sh`. The community package `pi-otel-telemetry` 1.0.0 (2026-03-09, cache) is the likely source; **not confirmed** (no provenance recorded).
- `pi-1` to `pi-3`: zero OTLP bodies. `pi-4` (with the extension): 3 bodies, `/v1/traces` and `/v1/metrics` as JSON. No auth header (the runner sets none). **run**: `runs/pi-4/http.jsonl`.
- Spans, scope `pi-otel-extension`: `agent.turn` x2 (carries `llm.usage.input_tokens`, `llm.usage.output_tokens`, `llm.usage.cache_*`, `turn.*`), `tool.read` (`tool.name`, `tool.args_summary`), `agent.prompt`, `session`. The normalizer gives all five kind `span`, no tokens, no provider. **run**: `runs/pi-4/norm.jsonl`.

### Devin SDK

- `@cognition-ai/sdk` 0.0.1-beta.9, published 2026-11-12T07:57:55Z (**run**, cache). Not run.
- **docs** (`pkgs/devin/node_modules/@cognition-ai/sdk/README.md`, "Tracing sessions"): `@cognition-ai/sdk/tracing` turns ACP sessions into OpenTelemetry spans; it needs no CLI flag; "your app owns the tracer provider, span processor and exporter". Spans carry Langfuse's OTLP attributes plus `gen_ai.*`. Prompts are recorded from every prompt path, including images and resources. Cloud sessions send no `usage_update`, so no `generation` spans.
- Verdict reasoning: nothing for `cetra agents setup` to write, since the exporter lives in the app's code.

### Omnigent

- Not installed. No package, no docs, no install log in E. No basis for a verdict. See § 7.

## 5. What cetra's normalizer does today, and proposed scope-gated rules

**run**: across all 117 normalized rows in these runs, none has `coding_agent` metadata or `cetra.span_name`. The current normalizer detects only Claude Code, Codex, Gemini CLI and Copilot (`crates/otel/src/agents/mod.rs`, `detect_agent`; **run** from this worktree). So every span above gets generic extraction only (conventions and kinds from attributes). Results:

- Usage: where `gen_ai.usage.*` (or `input_tokens`-style keys) exist, tokens are read: Qwen, Goose, Oh My Pi, Hermes, OpenCode v2. Pi's custom `llm.usage.*` keys are not read.
- Duplicates the normalizer keeps today: Goose's `stream_response_from_provider` repeats `chat`'s tokens (run: both kind `llm` with 82/17 and 131/12). Counted twice in any token sum. Hermes's `llm.*` wrapper repeats an `api.*` call without tokens (counted as an extra llm span).
- Noise kept today: OpenCode v1's `sql.execute` and `SQLiteDrizzle.make` (8 spans, run); Qwen's `@opentelemetry/instrumentation-undici` span and the spike's own `qwen-code.hook` spans.

Proposed rules, each gated by scope and service name, and each with a named test (none written; this is a spike):

| # | Gate (scope / service.name) | Rule | Evidence |
|---|---|---|---|
| R1 | service `opencode`, scope `com.opencode` | `opencode.llm` → llm (provider from attributes); `opencode.tool.<name>` → tool named `<name>`, original name to `cetra.span_name`; `opencode.session` → agent | `runs/opencode-v2/norm.jsonl` |
| R2 | service `opencode`, scope `opencode` (v1) | Drop `sql.*` and `SqliteDrizzle.*` spans (counted in `discarded`) | `runs/opencode-v1-builtin/norm.jsonl` |
| R3 | service `qwen-code` | `qwen-code.llm_request` → llm (already generic); `qwen-code.interaction` → agent; `qwen-code.tool` → tool; drop `qwen-code.hook` and scope `@opentelemetry/instrumentation-*` spans | `runs/qwen-final/norm.jsonl` |
| R4 | service `goose`, scope `goose` | Keep `chat` as the llm span with usage; drop `stream_response_from_provider` (child of `chat`) or drop its usage. Counted in `discarded`. Test: parent with child carries usage → one llm, tokens counted once. | `runs/goose-3/norm.jsonl` (parent links) |
| R5 | service `omp`, scope `@oh-my-pi/pi-agent-core` | `chat *` → llm; `execute_tool *` → tool; `invoke_agent` → agent. No drops. | `runs/omp-1/norm.jsonl` |
| R6 | service `hermes-agent`, scope `hermes-otel-plugin` (only if Hermes is first-class) | `api.<model>` → llm; `llm.<model>` without usage → drop; `tool.<name>` → tool; `agent` → agent | `runs/hermes-3/norm.jsonl` |
| R7 | service `pi`, scope `pi-otel-extension` (only if Pi is upgraded) | `agent.turn` → llm with `llm.usage.*` read as tokens; `tool.*` → tool; `agent.prompt` and `session` → agent. Needs a decision on reading custom keys. | `runs/pi-4/norm.jsonl` |

Detection (D-CA-3): each first-class agent needs a `CodingAgent` variant and a detection key in `detect_agent` (a service name or a scope). The keys above are the observed ones. The variant set is a shared-contract question (`cetra-core` is not changed by D-CA-3; the detection is in `cetra-otel`).

## 6. What `cetra agents setup <agent>` would need to write (proposed, mirroring D-CA-10)

D-CA-10 and the existing code: `setup.rs` writes user-level settings, `agents.toml`, key in the agent's own `.env` (Gemini), TOML plus hooks (Codex). Spans only, with logs and metrics exporters set to none. Content off by default (`--no-content`). Proposed for the first-class candidates:

- **Qwen Code**: `~/.qwen/settings.json` `telemetry` block (`enabled`, `target`, `otlpEndpoint` = cetra base, `otlpProtocol: http`, `logPrompts: false`, `includeSensitiveSpanAttributes: false`), and `~/.qwen/.env` with `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>`. Verified shape in `runs/qwen-dotenv`. Open: Qwen sends logs and metrics too and has no per-signal switch (§ 7).
- **OpenCode v2**: env-only in the evidence (`OPENCODE_ENABLE_TELEMETRY`, `OPENCODE_OTLP_ENDPOINT`, `OPENCODE_OTLP_PROTOCOL=http/protobuf`, `OPENCODE_OTLP_HEADERS`). Where OpenCode reads these from a file is not verified. The header needs a value form that OpenCode decodes (§ 4, § 7).
- **Goose**: env-only in the evidence (`OTEL_*`, plus `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` for content). The config-file route did not export. Where a setting can live is open (§ 7).
- **Oh My Pi**: env-only in the evidence, as Goose. Config-file route not tested.
- **Hermes Agent** (page only, if chosen): `config.yaml` plugin entry and `hermes_otel.yaml` backend, plus the plugin install that setup cannot do.
- **Pi, Devin SDK** (setup-only page): no settings file; Pi needs an extension file copied, Devin needs app code.
- **Augment Code** (undecided): `settings.json` `telemetry` and `env` block in its agent directory; the directory path is not verified.
- **OpenClaw, Omnigent**: nothing to write yet.

## 7. Open questions (need the user; nothing was decided here)

1. **Omnigent**: which package, product or URL is it? Nothing in E or the install logs names it. I did not guess.
2. **OpenCode build**: the npm 1.19.6 emitted no LLM spans in this run; the v2 binary (2.3.0, unrecorded source, not on npm as far as the spike knows) did. Which build do we support? Did the plugin `@devtheops/opencode-plugin-otel` emit the v2 spans? Which header form does OpenCode accept, given it does not decode `%20`?
3. **Cutoff interpretation (7-day rule)**: the installs used `npm --before 2026-11-17T23:59:59Z` (`tools/npmi.sh`), which admits all of 2026-11-17. The installed versions published on that date are: `opencode-ai` 1.19.6 (07:22Z), `openclaw` 2026.11.5 (09:59Z), `@augmentcode/auggie` 1.2.2 (04:46Z), `@qwen-code/qwen-code` 0.28.0 (12:10Z), `@oh-my-pi/pi-coding-agent` 18.22.1 (21:32Z). Hermes's code came from the git tag `v0.23.2` (commit 2026-11-16), so it is not in this list. If the rule is "published before 2026-11-17T00:00Z", these five are out and must be re-picked, and the runs above need repeating with those versions. Which reading does the user mean?
4. **Release dates not verified**: Goose 1.60.0, OpenCode v2.3.0, the OpenClaw diagnostics plugin, Hermes's `v0.23.2` tag date (only the commit date is known). A dated source needs a network read, which I did not do (§ 8).
5. **Qwen logs and metrics**: D-CA-10 says spans only. Qwen's docs give one `otlpEndpoint` and no per-signal switch (§ 4). Options: accept logs and metrics at cetra's OTLP endpoint (they are ignored or stored?), or add a decision. Which?
6. **Goose and OMP setup targets**: the only working route is environment variables. Is there a settings file that the agent reads for these, or does setup have to say "export these variables" (the `--print` mode in D-CA-10)?
7. **Pi**: reading its custom `llm.usage.*` keys would mean a scope-gated rule (R7), which is a product choice about a community extension. Keep Pi as setup-only?
8. **Hermes**: first-class or captured page? The telemetry is a community plugin, and setup cannot install it. Recommendation: captured page.
9. **Auggie**: a login-free run needs either a backend override or an offline mode. Is there one? Its telemetry also carries the user's email (`AUGMENT_USER_EMAIL`) in the resource: is that allowed (D-CA-5 uses the email as the person)?
10. **Devin SDK**: setup-only page is the design's word; confirm that "page with a snippet and no `agents setup` target" is what is meant. Its prompts are recorded by default (docs): the page must say so.
11. **Header variable name**: the Goose and Qwen runs send decoded `Bearer <key>`. The OpenCode run sends `%20` literally. The `cetra agents setup` writer must pick one form; that needs a decision once the OpenCode question is answered.

## 8. Network: what left loopback

**This session (run):** no network call left loopback. Every run was inside `unshare -Urn` (loopback only). Strace of the four new runs (`runs/hdr-omp`, `runs/hdr-goose`, `runs/hdr-oc`, `runs/hdr-hermes`): 141 `connect()` calls to IPv4, all to 127.0.0.1 (the fake and sink ports), none non-loopback, and no write syscalls under the real home. Earlier runs: `connect()` lines in `trace.strace` show only 127.0.0.1 and the 127.0.0.53 stub resolver inside the namespace (**run**). I did not run `tools/npmmeta.mjs`, `tools/pypimeta.mjs`, `tools/dl.mjs` or `npm` in this session. I did not use WebFetch.

**Previous agent (not sandboxed; these left loopback):**
- npm installs through `tools/npmi.sh`, with `NPM_CONFIG_USERCONFIG=/dev/null`, so the default registry (`registry.npmjs.org`) for `opencode-ai`, `openclaw`, `@augmentcode/auggie`, `@cognition-ai/sdk`, `@devtheops/opencode-plugin-otel`, `@oh-my-pi/pi-coding-agent`, `@earendil-works/pi-coding-agent`, `@qwen-code/qwen-code` and the `pi-ext` and `openclaw` dependencies (**run**: `tools/npmi.sh`, `logs/install-*.log`).
- Registry metadata reads through `tools/npmmeta.mjs` (`registry.npmjs.org`) and `tools/pypimeta.mjs` (`pypi.org`) (**run**: the scripts).
- Python venvs for Hermes (`pkgs/hermes-venv`, `pkgs/hermes-venv-314`, `logs/install-hermes*.log`). The index is **not recorded**; PyPI is the usual one.
- Git clones: `src/hermes-agent` from `github.com/NousResearch/hermes-agent` (tag `v0.23.2`) and `src/hermes-otel` from `github.com/briancaffey/hermes-otel` (tag `hermes-otel-v1.33.0`) (**run**: `.git/config` remotes). The clone commands are not recorded.
- Binary downloads with `tools/dl.mjs` (an arbitrary URL, `fetch` with redirects): `dl/goose/goose.tar.gz` (60.6 MB) and `dl/opencode-v2/opencode-v2.3.0.tar.gz` (115.9 MB). **The URLs were not recorded**, so I cannot name the hosts exactly. These are the largest unverified provenance gap.
- Tarballs in `tarballs/` for `opencode-ai`, `openclaw`, `hermes-agent`, the plugin packages, `cognition-sdk` and `pi-otel-telemetry`: the fetch method and source are not recorded.

None of these was a request to a real agent service or to a model provider. The runs used fake keys and the local fake provider.

## 9. Corrections to the previous agent's notes

- The header-logging concern was real for runs before 02:57:26. The fix is in `tools/server.py`. Post-fix header evidence exists for Qwen (`qwen-otelhdr-*`, `qwen-dotenv`), OMP (`hdr-omp`), Goose (`hdr-goose`), OpenCode v2 (`hdr-oc`), and Hermes (`hdr-hermes`). Pre-fix header results for any other run are invalid.
- Earlier per-run header summaries printed `auth_present=False` for OpenCode, Goose, Hermes, OMP and Pi. Those came from pre-fix runs and are not evidence. This spike's correction is above.
- `rtk` rewrites `grep` in the Bash tool and truncates output, so some earlier greps in this session were not raw. Results above come from `python` reads.
