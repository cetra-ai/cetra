# M7 spike: setup routes for OpenCode v2, Goose, Oh My Pi

Status: spike, not a plan. Continues `m7-coding-agents.md` and corrects it where the runs below disagree. Nothing here changes the design or the decisions. The open questions in § 7 need the user. Written 2026-11-24, clock 09:30 UTC.

Evidence root (E): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7`. Paths below are relative to E unless absolute. This session's evidence is in `m7c/` (runs in `m7c/runs/`, driver `m7c/drive.sh`, analyzer `m7c/analyze.py`); runners are `runners/m7c-*.sh`. Each fact is tagged:

- **run**: observed by a command in this session, with the evidence path.
- **docs**: read from the package's own source in E (the path and line are given), or from a release page.

## 1. Scope

- OpenCode v2 (`v2.3.0`), Goose (`v1.59.2`), Oh My Pi (`18.22.0`): versions pinned, routes and signals tested, normalizer output recorded.
- Not re-run here: Qwen Code, Hermes Agent, OpenClaw, Augment Code, Devin SDK, Omnigent, Pi (the earlier spike's verdicts stand for them).

## 2. Versions (the 7-day rule)

Rule: the newest release published at least 7 days before install. Install clock: 2026-11-24 09:30 UTC, so the cutoff is **2026-11-17 09:30:10 UTC**. A release published after the cutoff is not eligible.

| Item | Version pinned | Asset or package (URL) | Published (UTC) | sha256 / integrity | Notes |
|---|---|---|---|---|---|
| OpenCode v2 | `v2.3.0` | `https://github.com/anomalyco/opencode/releases/download/v2.3.0/opencode-linux-x64.tar.gz` (121,569,810 bytes) | 2026-11-13T08:55:07Z (GitHub release `v2.3.0`, not a pre-release) | `fc0f81522aceeabac47693d88389efa2bc517ce74c2c38fd6967819fcc6e4d3b` (matches the release's `SHA256SUMS` and the API digest) | Repo `anomalyco/opencode`. Extracted: `m7c/bin/oc-2.3.0/opencode`. Source archive `https://github.com/anomalyco/opencode/archive/refs/tags/v2.3.0.tar.gz`, sha256 `48b0ab7c…` (equal to the earlier spike's `m7c/src/opencode-v2.3.0-src.tar.gz`). |
| OpenCode, newest eligible overall (not used) | `v1.19.6` | GitHub release `v1.19.6` (npm `opencode-ai` 1.19.6) | 2026-11-17T07:21:32Z | release sha256 `cef86c57…` (not verified here) | Eligible, but the 1.x line sends no model spans (`m7-coding-agents.md` § 4). Not pinned. |
| Goose | `v1.59.2` | `https://github.com/aaif-goose/goose/releases/download/v1.59.2/goose-x86_64-unknown-linux-gnu.tar.gz` (62,461,010 bytes) | 2026-11-16T05:26:30Z | `c63cf82953e968d84b7542eb38a621dfdb5050b967728ed363517be5cdc666ea` (equal to the API digest; the earlier local copy `m7c/dl/goose-v1.59.2-…tar.gz` has the same digest) | Extracted: `m7c/bin/goose-1.59.2/goose`; `goose --version` → `1.59.2` (run: `m7c/runs/g159-env/run.log`). Source archive `https://github.com/aaif-goose/goose/archive/refs/tags/v1.59.2.tar.gz`, sha256 `d12f7c72…`, unpacked at `m7c/src/goose-src-1.59.2/goose-1.59.2`. |
| Oh My Pi | `18.22.0` | npm `@oh-my-pi/pi-coding-agent@18.22.0`, `https://registry.npmjs.org/@oh-my-pi/pi-coding-agent/-/pi-coding-agent-18.22.0.tgz` | 2026-11-16T22:10:38.648Z (registry `time` field) | `sha512-gAsi1C6X…` (`m7c/pkgs/omp-18.22.0/package-lock.json`) | Install: `m7c/pkgs/omp-18.22.0`. `omp --version` → `omp/18.22.0` (run: `m7c/runs/omp-openai/run.log`). |
| Bun (runtime for Oh My Pi, a dependency) | `1.4.4` | npm `bun@1.4.4` | 2026-10-22T10:19:13.548Z | `sha512-hj0ASdG6…`; installed `bun.exe` sha256 `10702245766b225e70bbf6235fd6b47214fd94b4760d00ffaed8ab0df57ac235` | The newest eligible release that satisfies `>=1.4.4`. 1.4.5 was published 2026-11-19 (not eligible). |

Ineligible releases (for the record):

- Goose `v1.60.0` (2026-11-17T23:08:54Z), `v1.60.1` (2026-11-18), `v1.60.2` (2026-11-20). The earlier spike ran 1.60.0 (`runs/goose-3`); it is too new.
- OpenCode `v2.3.1` (2026-11-18T00:17:57Z), `v2.3.2` (2026-11-18T01:35:12Z), `v2.4.0` and `v2.4.1` (2026-11-23).
- Oh My Pi `18.22.1` (2026-11-17T21:32:54Z; the earlier spike ran it), `19.1.1` (2026-11-23).

Source of dates: `m7c/sst-opencode-releases.json` and `m7c/goose-releases.json` (GitHub release lists, read by the earlier agent); `m7c/omp-time.json` and `bun-time.json` (`npm view … time --json`, run this session).

The earlier cutoff (`tools/npmi.sh`, `--before 2026-11-17T23:59:59Z`) admits everything published on 2026-11-17, which is inside the 7-day window. It is wrong for this rule. See § 9.

## 3. Method

- Each run executes inside `unshare -Urn` (loopback only), via `tools/netrun.sh`, with the runner under `runners/m7c-*.sh`. Fake keys only (`sk-fake-m7`). The fake provider and OTLP sink (`tools/server.py`) run on loopback in the namespace. **run**: `m7c/runs/*/trace.strace`, all `connect()` calls to 127.0.0.1.
- New runs: `m7c/drive.sh <name> <runner> <bin> <timeout>`. It creates `runs/<name>/`, links the verified binary into `runs/<name>/bin/`, runs the case, then normalizes the received bodies with cetra's real `normalize` (`norm/`, binary built earlier, not rebuilt; the worktree's `crates/otel`).
- Analysis: `m7c/analyze.py` (OTLP posts by signal and content type, the Authorization header as received, and each llm, tool and agent span with its parent, provider, model, tokens, and I/O presence).
- No cargo build, nothing built in the repo.
- Source reads: `m7c/src/opencode-src`, `m7c/src/goose-src-1.59.2/goose-1.59.2`, `m7c/pkgs/omp-18.22.0/node_modules/@oh-my-pi/pi-coding-agent/src` (with `…/pi-utils/src`), and `pkgs/ocplugin-bundle/plugin.js` (the community plugin, minified).

## 4. Run index

| Run (`m7c/runs/`) | What it tests | Result |
|---|---|---|
| `oc230-noplugin` | OpenCode v2.3.0 binary only, plain-space header | 3 trace and 4 log posts (JSON), 698 spans |
| `oc230-noplugin-pct` | Same, header `Bearer%20…` | Header received literally (19 characters) |
| `oc230-plugin` | Same binary with the community plugin (`OPENCODE_*` names, `%20` header) | 4 spans, scope `com.opencode`, protobuf |
| `oc230-plugin-nocap` | Plugin without `OPENCODE_CAPTURE_MODEL_CONTEXT` | Same 4 spans; I/O still present |
| `g159-env` | Goose 1.59.2, env route, plain-space header, content on | 6 spans; traces, metrics and logs all posted |
| `g159-cfg-hdr` | Goose, endpoint and timeout in `config.yaml` (lowercase keys), header in env | Same 6 spans; header decoded |
| `g159-cfg-noenv` | Goose, endpoint in `config.yaml` only (no OTEL env) | Same 6 spans; no Authorization sent |
| `g159-signals-off` | Goose, `OTEL_LOGS_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none` | Traces only |
| `omp-openai` | Oh My Pi 18.22.0, provider named `openai`, env route | 7 spans, `gen_ai.provider.name` = `openai`, content on |
| `omp-optout` | Oh My Pi, `telemetry.otlpExportEnabled: false` in `~/.omp/agent/config.yml` | 0 OTLP posts |
| `omp-cfg-endpoint` | Oh My Pi, endpoint and header in `config.yml`, no env | 0 OTLP posts |
| `omp-signals-off` | Oh My Pi, logs and metrics set to `none` | Traces only |
| `omp-dotenv-agentdir` | Oh My Pi, endpoint and header in `~/.omp/agent/.env`, no env | Traces, metrics and logs posted; header decoded |
| `omp-dotenv-project` | Same lines in the project `.env` (the work dir), no env | Same, posted |
| `omp-dotenv-content` | `~/.omp/agent/.env` with endpoint, header, `OTEL_SERVICE_NAME=omp` and the content switch | 7 spans, I/O present, service `omp` |

## 5. Findings per agent

### 5.1 OpenCode v2.3.0

**Which code emits what (run and docs).**

- The binary alone (`oc230-noplugin`, run) emits its own spans and logs. Scope `effect`, service `opencode` (resource `serviceName: "opencode"`, docs `packages/util/src/observability/otlp.ts:51`). Its spans number 698: 693 generic and 5 GenAI-convention rows. Most are Effect internals (`PluginHost.make` 94, `Plugin.load` 94, `FileSystem.readFileStringSafe` 53, `State.notify` 22, `AppProcess.run` 21, and so on).
- Its model-related spans are `SessionStep.attempt` (llm; carries `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`/`output_tokens`/`cache_read.input_tokens`; tokens 82/17 and 131/12), `LLM.stream` (llm, one per call, no tokens) and `Tool.execute` (tool, `gen_ai.tool.name`, status error). **run**: `m7c/runs/oc230-noplugin/norm.jsonl`.
- `opencode.session`, `opencode.llm` and `opencode.tool.read` appear **only** with the community plugin (scope `com.opencode`, 4 spans in total, `oc230-plugin`). The binary never emits those names. **run**: `m7c/runs/oc230-plugin/norm.jsonl`, and `oc230-noplugin` has no such name.
- The binary's source has `opencode.tool.<name>` strings only as tool-plugin ids (`packages/core/src/tool/plugin/*.ts`, e.g. `id: "opencode.tool.edit"`), not as span names. **docs**.
- The `OPENCODE_ENABLE_TELEMETRY`, `OPENCODE_OTLP_ENDPOINT`, `OPENCODE_OTLP_PROTOCOL`, `OPENCODE_OTLP_HEADERS`, `OPENCODE_CAPTURE_*` and `OPENCODE_DISABLE_{LOGS,METRICS,TRACES}` names occur in **no** file of the v2.3.0 tree. They occur in the plugin bundle `pkgs/ocplugin-bundle/plugin.js` (resolver of `enabled`, `logsEnabled`, `capturePromptInLogs`, `captureModelContext`). **docs** (`m7c/src/opencode-src`: grep, no hit; `plugin.js`: hits).

**Answer to the attribution question.** The `opencode.*` spans and the `OPENCODE_*` settings come from the community plugin `@devtheops/opencode-plugin-otel`. The binary's own route is the standard `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS` (docs, `packages/util/src/observability.ts:21-24` and `packages/cli/src/index.ts:151-153`), and it emits different spans (scope `effect`). The design's wording "the v2 build's telemetry environment variables (`opencode.llm`, tool and session spans)" (`00-design.md`, the Coding agents table; D-IN-12 and D-IN-27 point to it) is therefore wrong for the binary route. See § 7 item 1.

**Environment and config (docs).**

- Binary env: `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS` only (`packages/util/src/observability.ts:21-24`). It reads no `OTEL_EXPORTER_OTLP_PROTOCOL` and no per-signal endpoint. Its exporters post JSON (run: `application/json` on `/v1/traces` and `/v1/logs`).
- Header parsing: split on `,`, then on `=` (`packages/util/src/observability/otlp.ts:16-27`). No percent-decoding.
- Config file: no telemetry or OTLP key in `packages/schema/src/config.ts` (the only telemetry mention is a `username` description, line 53). **docs**.
- Dotenv: no loader for OTEL names in `packages/{cli,core,util,server,app}/src` (grep hits only in `mcp/stdio.ts` and tests). **docs**, not run.
- Conclusion: **environment only**. No file route found.

**Header form (run).**

- `Authorization=Bearer sk-fake-m7` (plain space, `oc230-noplugin`): received as `Bearer sk-fake...` (17 characters), decoded correctly.
- `Authorization=Bearer%20sk-fake-m7` (`oc230-noplugin-pct`): received literally as `Bearer%20sk-fake...` (19 characters). The binary does not decode `%20`.
- The first spike's `%20` literal came from the plugin route (`oc230-plugin`, 19 characters) and from the binary too. The plain-space form works for the binary and for Goose and Oh My Pi (§ 5.2, 5.3).

**Content switch.**

- Binary: none. No `process.env` name in the binary's source controls prompt or content capture (docs: grep of `packages/core/src` and `packages/*/src`). The binary's spans carry no I/O (run: `io=-/-` on every llm and tool row).
- Plugin: I/O is present on its llm spans **with** and **without** `OPENCODE_CAPTURE_MODEL_CONTEXT` (`oc230-plugin`, `oc230-plugin-nocap`, run). The plugin's capture switches are `OPENCODE_CAPTURE_PROMPT_IN_LOGS` and `OPENCODE_CAPTURE_MODEL_CONTEXT` (docs, `plugin.js`). Which one controls span I/O was not isolated.

**Logs and metrics.**

- Binary: logs are always sent once an endpoint is set (`loggers()` returns the OTLP logger when `options.endpoint` is set, docs `packages/util/src/observability/otlp.ts:63-72`); there is no switch except unsetting the endpoint, which also drops traces. Run: 4 log posts in `oc230-noplugin`. No metric posts from the binary.
- Plugin: logs, metrics and traces each have a disable switch (`OPENCODE_DISABLE_LOGS`, `OPENCODE_DISABLE_METRICS`, `OPENCODE_DISABLE_TRACES`, docs `plugin.js`). Run: metrics and logs posted (`oc230-plugin`).

**Normalizer today (run).**

- Binary route (`oc230-noplugin`): 698 rows kept, 0 discarded. 4 llm rows = 2 real calls counted twice (`SessionStep.attempt` with tokens, `LLM.stream` without); 1 tool row; 693 generic `span` rows.
- Plugin route (`oc230-plugin`): 4 rows: `opencode.session` (agent), `opencode.llm` x2 (provider `openai`, model `gpt-4o-mini`, tokens 82/17 and 131/12, I/O on the second), `opencode.tool.read` (tool, status error).

**Process isolation (run).** Real-home: none (`m7c/runs/oc230-*/real-home.txt`).

### 5.2 Goose v1.59.2

**Routes (run and docs).**

- Env (`g159-env`): `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`. Traces, metrics and logs (protobuf) all posted.
- Config file (`g159-cfg-hdr`, `g159-cfg-noenv`, run): `config.yaml` with **lowercase** keys `otel_exporter_otlp_endpoint: <url>` and `otel_exporter_otlp_timeout: <secs>` works. The source promotes exactly these two keys to env, and only if the env var is unset (docs `crates/goose/src/otel/otlp.rs`, `promote_config_to_env`, lines 261-271, called at line 303, before the exporters are built at lines 327, 379, 418). The earlier spike's failed config run (`runs/goose-config`) wrote uppercase keys (`OTEL_EXPORTER_OTLP_ENDPOINT:`), which Goose does not read from the file: that negative result was a wrong key name (§ 9).
- Headers: **env only**. No config key (`promote_config_to_env` handles endpoint and timeout only). `g159-cfg-noenv` sent no Authorization header (run).
- Content: **env only**. `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`, read by `std::env::var` (docs `crates/goose/src/agents/gen_ai_telemetry.rs:9-12`). No config key.
- Logs and metrics: `OTEL_LOGS_EXPORTER=none` and `OTEL_METRICS_EXPORTER=none` (env only, docs `otlp.rs` `signal_exporter`, lines ~224-260). Run `g159-signals-off`: traces only.
- Service name: `goose`, set in code (`create_resource`, `service.name` = `goose`; docs `otlp.rs`). `OTEL_SERVICE_NAME` overrides it (docs). Scope name: `goose`.
- Config path: `$XDG_CONFIG_HOME/goose/config.yaml` if set, else `~/.config/goose/config.yaml` (docs `crates/goose/src/config/base.rs:86`, `170`; run uses the XDG path).

**Header form (run).** Plain space: received as `Bearer sk-fake...` (17 characters, decoded). The earlier spike's `%20` form was also decoded (`hdr-goose`, earlier).

**Spans and attribute keys (run, `g159-env`, cetra's normalizer).** Six rows, all in scope `goose`:

| Span | Kind | Parent | Usage | I/O | Keys (gen_ai and other) |
|---|---|---|---|---|---|
| `reply` | agent | none | 213 input, 29 output (the sum of both turns) | in and out | `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.agent.name`, `gen_ai.operation.name`, `session.id`, `trace_input`, `trace_output`, `user_message`; normalizer warning `duplicate_key` |
| `chat` (x2) | llm | `reply` | 82/17 and 131/12 | none | `gen_ai.operation.name` (chat), `gen_ai.provider.name` (openai), `gen_ai.request.model`, `gen_ai.request.max_tokens`, `gen_ai.response.model`, `gen_ai.response.id`, `gen_ai.response.finish_reasons`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| `stream_response_from_provider` (x2) | llm | its `chat` | the same 82/17 and 131/12 | in and out | the same `gen_ai.*` keys as `chat`, plus `gen_ai.conversation.id`, `gen_ai.request.stream`, `session.id` |
| `execute_tool` | tool | `reply` | none | in only | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.conversation.id`, `error.type` |

**The double-count, precisely.** `stream_response_from_provider` is a `#[tracing::instrument]` function (docs `crates/goose/src/agents/reply_parts.rs`, the attribute above `stream_response_from_provider`, about lines 224-247) that sets the same `gen_ai.operation.name = "chat"` and the same usage as its parent `chat`. The usage is on both spans; the I/O is only on the child. The normalizer keeps both as llm (run). Any token sum over llm spans counts each call twice. Also, `reply` carries the total usage, so a sum that includes the agent span counts it a third time. The normalizer warns `duplicate_key` on `reply` (run; code `crates/otel/src/warn.rs`).

**1.59.2 vs 1.60.0.** The span set is the same as the earlier 1.60.0 spike (`runs/goose-3`, 6 spans, same parents).

### 5.3 Oh My Pi 18.22.0

**Routes (docs and run).**

- Env: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (`omp-openai`, `omp-signals-off`, run).
- Dotenv files: OMP reads every `OTEL_*` name from `~/.env`, `~/.omp/.env` and `~/.omp/agent/.env` into a private map, and uses it when no live env value exists (docs `pi-utils/src/env.ts` lines 286-292 for the parsed files and 312-347 for the map and `$otelEnv`). Run: `~/.omp/agent/.env` with endpoint, header, service name and content works (`omp-dotenv-agentdir`, `omp-dotenv-content`). The **project** `.env` (in the working directory) also works (`omp-dotenv-project`, run). The source comment says project files are kept out of the environment, but the project `.env` values did reach the exporter.
- Settings file: `telemetry.otlpExportEnabled` (boolean, default `true`, docs `src/telemetry-settings.ts`), in the agent dir's `config.yml` (`~/.omp/agent/config.yml`). Setting it to `false` posts nothing (`omp-optout`, run).
- The endpoint and headers in `config.yml` do nothing (`omp-cfg-endpoint`, run: no posts). `config.yml` is for the opt-out only.
- Protocol: only `http/protobuf`. `grpc` or `http/json` disables the signal with a warning (docs `src/telemetry-export.ts`, `signalEnabled`). Not run.
- Signals: traces, logs and metrics are all on by default (run: `omp-openai`). `OTEL_LOGS_EXPORTER=none` and `OTEL_METRICS_EXPORTER=none` leave traces only (run: `omp-signals-off`).
- Service name: `omp` with `OTEL_SERVICE_NAME=omp` (run); without it, the fallback is `oh-my-pi` (docs `src/telemetry-export-otlp.ts:55`, not run).

**Header form (run).** Plain space: received as `Bearer sk-fake...` (17 characters, decoded).

**Provider (run).** With a provider named `openai` in `models.yml` (`openai/gpt-4o-mini`), every `chat` span has `gen_ai.provider.name = openai` and `gen_ai.request.model = gpt-4o-mini`. **Yes**: the provider is `openai` (run: `omp-openai`, attribute dump of one chat span).

**Spans (run, `omp-openai`).** Scope `@oh-my-pi/pi-agent-core`, service `omp`:

- `invoke_agent` (agent, no tokens at top level; the `omp.gen_ai.agent.usage.*.total` attributes hold the sum).
- `chat gpt-4o-mini` x5 (llm, tokens 82/17 and 131/12, I/O with the content flag). Five model POSTs reached the fake, so five spans, no duplicate. Three of the five have no parent in the export; the cause was not checked (§ 7).
- `execute_tool read` (tool, I/O with the content flag, status `error` in every run including `omp-dotenv-*`; the fake provider's tool arguments are the likely cause, not checked).

**Content (run).** Without the content flag: no I/O (`omp-1`, earlier). With it, I/O is on every llm and tool span (`omp-openai`, `omp-dotenv-content`).

**Normalizer today (run).** 7 rows: 5 llm, 1 tool, 1 agent; provider `openai`, model `gpt-4o-mini`, tokens on llm rows, convention `genai`, no duplicates.

**Real home (run).** Reads only: OMP's discovery touches `~/.claude-extra`, `~/.git`, `~/.jj` and `~/.agents` (`m7c/runs/omp-openai/real-home.txt`). `strace` shows no write-mode open, rename or unlink under `/home/sebastien`.

## 6. Setup facts (what `cetra agents setup` would write or print)

Existing behaviour, for comparison (docs, `crates/cli/src/agents/setup.rs`): Copilot prints `export` lines and writes nothing (`copilot`, line 679); its header line is `Authorization=Bearer%20<key>` (line 694) and its content line is `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY` (line 700). Gemini CLI writes a marked header line into its own `.env` (`gemini_header_line`, line 539, `gemini_cli_setup`, line 589). Neither applies to these three agents as written.

Proposed facts, each from a run or a source read above. They are inputs to the plan, not decisions.

**OpenCode v2.3.0** (route: environment only; no file route found):

- No file is written. The setup can print: `export OTEL_EXPORTER_OTLP_ENDPOINT=<url>` and `export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer <key>'` (plain space; `%20` would be sent literally).
- Logs are always sent with an endpoint; the spike cannot turn them off. Content: none.
- The Copilot print's `%20` form must not be reused for OpenCode.
- Service `opencode`, scope `effect` for the binary route; scope `com.opencode` for the plugin route.

**Goose 1.59.2** (route: `config.yaml` for the endpoint and timeout, environment for the rest):

- `~/.config/goose/config.yaml` (or `$XDG_CONFIG_HOME/goose/config.yaml`): `otel_exporter_otlp_endpoint: <url>` and `otel_exporter_otlp_timeout: <secs>`.
- Printed as `export`, since the config cannot hold them: `OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer <key>'`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (content on), and for traces only: `OTEL_LOGS_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none`.
- Service `goose`, scope `goose`.

**Oh My Pi 18.22.0** (route: a dotenv file the Gemini pattern can write, plus a settings opt-out):

- `~/.omp/agent/.env`: `OTEL_EXPORTER_OTLP_ENDPOINT=<url>`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>`, `OTEL_SERVICE_NAME=omp`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (content on). Verified by `omp-dotenv-content`.
- Logs and metrics off: `OTEL_LOGS_EXPORTER=none` and `OTEL_METRICS_EXPORTER=none` in the same file (run `omp-signals-off` used the env; the dotenv form was not run).
- Remove: the setup's marked lines, or the whole file if it only holds them. Also the `telemetry.otlpExportEnabled: false` opt-out in `~/.omp/agent/config.yml` (run `omp-optout`) if the user opts out.
- Service `omp`, scope `@oh-my-pi/pi-agent-core`.
- Caution: the project `.env` also carries these values (run `omp-dotenv-project`). A repository's `.env` could therefore route its telemetry; setup should not write there.

## 7. Open questions (need the user)

1. **OpenCode route.** Two routes exist and the design names neither correctly.
   - (a) The binary's own route: env only, JSON, scope `effect`, 698 spans per short run of which 4 are model rows (two calls each counted twice), logs that cannot be turned off, and GenAI attributes on `SessionStep.attempt` (tokens) and `Tool.execute`. It needs discard rules for the Effect internals.
   - (b) The community plugin `@devtheops/opencode-plugin-otel`: its own `OPENCODE_*` env names, protobuf, metrics and logs with switches, and a clean 4-span output (`opencode.session`, `opencode.llm`, `opencode.tool.*`). It is a third-party install that `agents setup` cannot make, the same problem that gave Hermes a page (D-IN-27 item 3).
   - Which route does the user want? The design's D-IN-12 and D-IN-27 wording ("the v2 build's telemetry environment variables") needs correcting for either.
2. **Plugin spans replace binary spans?** With the plugin loaded, the binary's 698 Effect spans did not reach the sink (`oc230-plugin`: 4 spans). Cause not found. It matters only if route (b) is chosen with the binary's spans also wanted.
3. **OpenCode logs.** The binary always posts logs when an endpoint is set. D-CA-10 says spans only. Accept and drop them at the receiver, as D-IN-27 item 5 does for Qwen?
4. **OpenCode JSON.** The binary posts OTLP JSON (`application/json`). The normalizer decodes it (`norm`, `decode_json`). Whether cetra's server accepts JSON OTLP on `/v1/traces` was not checked here; the plan must check it.
5. **Goose double count.** Keep `chat`'s usage and drop `stream_response_from_provider`, which has the I/O? Or copy the I/O onto `chat`? And `reply`'s total usage: skip it for token sums? The plan decides, but the choice must be made from these keys.
6. **Oh My Pi setup target.** `~/.omp/agent/.env` (the Gemini pattern) or print-only? And should setup also write the `telemetry.otlpExportEnabled` opt-out for `--no-export`?
7. **Oh My Pi parentless chat spans.** Three of five `chat` spans have no parent in the export. The cause is not known (not the fake: five POSTs, five spans, no duplicates). It affects a span-tree rule, not token counts.
8. **Header and content variable names in the existing Copilot print.** `setup.rs:694` uses `%20` and `setup.rs:700` uses `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY`. For OpenCode the `%20` form is wrong; Goose and Oh My Pi use `…_CAPTURE_MESSAGE_CONTENT=true` (verified). Is Copilot's name right? Out of M7's scope, but the setup writers should share one header form.
9. **Plugin version.** The plugin's version was not recorded by the earlier install (`logs/install-ocplugin*.log`). Needed only if route (b) is chosen; it would then be pinned under the 7-day rule.
10. **Tool-span status `error`.** Every run's `read` tool reports an error (OMP, OpenCode binary `Tool.execute`, plugin `opencode.tool.read` with "File not found"). The fake provider's arguments are the likely cause. Not checked; it affects only the fixture's realism, not the routes.
11. **OpenCode dotenv.** The grep found no OTEL dotenv loader in the packages named in § 5.1. A run with a project `.env` was not done.

## 8. Corrections to `m7-coding-agents.md`

- **OpenCode, route.** The `OPENCODE_*` variables are the plugin's, not the binary's. The binary uses `OTEL_EXPORTER_OTLP_*` and emits scope `effect` spans (§ 5.1).
- **OpenCode, header.** The binary also sends `%20` literally, not only the plugin. The plain-space form works for the binary.
- **Goose, config route.** It works with the lowercase keys `otel_exporter_otlp_endpoint` and `otel_exporter_otlp_timeout`. The earlier "did not export" came from uppercase keys (§ 5.2).
- **Goose, versions.** 1.60.0 is too new under the 7-day rule. The pinned version is 1.59.2 (same span set).
- **Oh My Pi, versions.** 18.22.1 is too new. The pinned version is 18.22.0, with bun 1.4.4.
- **Oh My Pi, routes.** Besides env, there is a dotenv file route (`~/.omp/agent/.env`, and project `.env`), and a settings opt-out (`telemetry.otlpExportEnabled`). Endpoint in `config.yml` does nothing.
- **Oh My Pi, protocol.** Only `http/protobuf` is accepted (`grpc` and `http/json` are declined).
- **Goose duplicate.** The `reply` agent span also carries the total usage (213/29). The earlier table did not record it.
- **Cutoff.** The earlier install used `--before 2026-11-17T23:59:59Z`, which admits releases inside the 7-day window. `tools/npmi.sh` should not be reused for pins; this spike used exact versions and checked the publish times.

## 9. Network

**Outside loopback (all deliberate, all recorded):**

- npm registry reads this session: `npm view bun time --json` and `npm view @oh-my-pi/pi-coding-agent time --json` (`registry.npmjs.org`; outputs `m7c/bun-time.json`, `m7c/omp-time.json`).
- npm install this session: `@oh-my-pi/pi-coding-agent@18.22.0` and `bun@1.4.4` into `m7c/pkgs/omp-18.22.0` (`registry.npmjs.org`; `m7c/logs-install-omp18220-pinned.log`, `package-lock.json` holds the integrity hashes).
- GitHub downloads this session (`curl`, official repos only):
  1. `https://github.com/anomalyco/opencode/releases/download/v2.3.0/opencode-linux-x64.tar.gz` (121,569,810 bytes), into `m7c/dl/oc-2.3.0/`;
  2. `https://github.com/anomalyco/opencode/releases/download/v2.3.0/SHA256SUMS` (2,544 bytes), into the same folder;
  3. `https://github.com/aaif-goose/goose/releases/download/v1.59.2/goose-x86_64-unknown-linux-gnu.tar.gz` (62,461,010 bytes), into `m7c/dl/goose-1.59.2-gh/`;
  4. `https://github.com/anomalyco/opencode/archive/refs/tags/v2.3.0.tar.gz` (source, 71 MB), into `m7c/src/oc-src-2.3.0/`;
  5. `https://github.com/aaif-goose/goose/archive/refs/tags/v1.59.2.tar.gz` (source, 302 MB), into `m7c/src/goose-src-1.59.2/`.
  Each sha256 is in § 2.
- Earlier (previous agent, not re-run): the GitHub release JSON files in `m7c/*.json` and the unrecorded downloads (`dl/opencode-v2/opencode-v2.3.0.tar.gz`, 115.9 MB, provenance unknown; not used here).

**Inside the namespace (all runs in this session):** every `connect()` in the 15 new runs went to 127.0.0.1 (`trace.strace`: 0 non-loopback connects in each run). Fake keys only (`sk-fake-m7`). No real provider was contacted.

**Real home:** the Oh My Pi runs read paths under `/home/sebastien` (§ 5.3) and wrote none. The other runs touched no real-home path (`real-home.txt` empty).

**Processes:** I started and stopped only the runs' own processes (`timeout` and the namespace wrapper). I killed nothing.

**Not done:** no commit, no push, no build in the repo, no edits outside E and this file.

## 10 Built-in telemetry with experimental.openTelemetry

Written 2026-11-24, clock 09:50 UTC. Evidence in `m7d/` under E (`m7d/runs/<name>/`, analyzers `m7d/inv.py` and `m7d/rawinv.py`, raw inventories `m7d/raw-*.txt`, runner `m7d/runners/m7d-oc.sh`, driver `m7d/drive.sh`). Question: LangWatch documents `experimental.openTelemetry: true` in `~/.config/opencode/opencode.jsonc` plus the standard OTEL variables, and says OpenCode then sends tool arguments and results. § 5.1 credited the `com.opencode` spans to the community plugin, which this section does not use. Tags as in § 1.

### 10.1 Versions and downloads

- **OpenCode v2.3.0**: as § 2. The extracted binary `m7c/bin/oc-2.3.0/opencode` has sha256 `2e0e4656…381f`. The release tarball `m7c/dl/oc-2.3.0/opencode-linux-x64.tar.gz` has sha256 `fc0f8152…4d3b` (**run**: `sha256sum`, equal to the § 2 value). No download this section.
- **OpenCode 1.x (npm `opencode-ai`)**: the newest stable release published before 2026-11-17T10:00Z is **1.19.6**, published **2026-11-17T07:22:20.263Z** (**run**: `npm view opencode-ai time --json`, `m7d/opencode-ai-time.json`; `npm view` on the host, registry.npmjs.org). Later entries are `0.0.0-dev-…` snapshots (2026-11-17T07:46Z and after), not releases, so they are not eligible. `dist.integrity` of `opencode-ai@1.19.6` is `sha512-x0yTvlq3…GwA==` and of `opencode-linux-x64@1.19.6` is `sha512-SUQS023u…1A==` (**run**: `npm view`). The copy already in `pkgs/opencode` is the same version (`node_modules/opencode-ai/package.json` `"version": "1.19.6"`), so nothing was reinstalled. Binary `pkgs/opencode/node_modules/opencode-linux-x64/bin/opencode`, sha256 `aab0a58c…1173`.

### 10.2 Where the flag is read

**v2.3.0: the flag is rejected, not read (docs, with run confirmation).**

- `packages/core/src/config/normalize.ts:46-51`: `unsupportedExperimental` lists `"openTelemetry"`.
- `normalize.ts:459-461`: each listed key goes through `unsupportedIfPresent`.
- `normalize.ts:799-802`: that function pushes a diagnostic `kind: "unsupported"`, message "omitted unsupported legacy setting", and the key is not carried into the normalized config.
- `packages/schema/src/config/experimental.ts`: `ConfigExperimental.Info` has no `openTelemetry` field (only `portable_shell_scanner`, `codemode_vm`, `all_flags`, the flag registry, `part_rows`, `image_generate`, `subagent_depth`, `location_idle_timeout`, `policies`).
- `packages/core/test/config/normalization.test.ts:683-700` asserts the flag is reported as `unsupported` (`["experimental", "openTelemetry"]`).
- No `experimental_telemetry` in `packages/*/src` (grep, no hit). The v2 binary contains neither `openTelemetry:` nor `experimental_telemetry` nor `experimental?.openTelemetry` (byte grep of the binary, 0 hits).
- Run: `m7d/runs/oc230-flag` (flag set) and `oc230-cfgonly` (a global config without the flag) both give 702 spans, the same attribute keys (`m7d/raw-oc230-flag.txt`, `raw-oc230-flag-contentnames.txt`), and no `ai.*` attributes. The flag changes nothing.
- The 4 extra `Config.parseInfo` and `ConfigVariable.substitute` spans in the flag runs versus `oc230-noflag` (698 spans) come from the presence of a global config file, not from the flag (**run**: `oc230-noflag` has no global file; `oc230-cfgonly` matches `oc230-flag`).

**1.19.6: the flag is read and turns on the AI SDK's telemetry (binary strings, minified bundle; no source).**

- Schema: `openTelemetry:c(z).annotate({description:"Enable OpenTelemetry spans for AI SDK calls (using the 'experimental_telemetry' flag)"})` at byte offset 110414828 of the binary.
- Reads: `s.experimental?.openTelemetry` (offsets 126073976, 126074261) feeds `experimental_telemetry:{isEnabled:s.experimental?.openTelemetry,tracer:w,metadata:{userId,…}}` in the session prompt path. `L.experimental?.openTelemetry` (offsets 126815229, 126817569) builds the tracer and sets `experimental_telemetry:{isEnabled:L.experimental?.openTelemetry,functionId:"session.llm",tracer:C,…}`. The tracer factory names (`an`, `Ws`) are minified and were not identified.
- Run: `npm119-flag` gives 3 `ai.streamText.doStream`, 3 `ai.streamText` and 1 `ai.toolCall`. `npm119-noflag` (same command, no flag) gives 0 `ai.*` spans (`m7d/raw-npm119-flag.txt`; norm and raw counts).

### 10.3 Span inventory

**v2.3.0 with the flag** (`m7d/runs/oc230-flag`, **run**): 702 spans, 31 traces for one turn, scope `effect`, service `opencode`. Kinds: internal 687, server 12, client 3.

| Span | Kind | Count | Parent | GenAI and other attributes (raw) |
|---|---|---|---|---|
| `SessionStep.attempt` | llm (normalizer) | 2 | `SessionRunner.runStep` | `gen_ai.provider.name`=openai, `gen_ai.request.model`=gpt-4o-mini, `gen_ai.usage.input_tokens` 82 / 131, `gen_ai.usage.output_tokens` 17 / 12, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.response.finish_reasons`, `gen_ai.agent.name`, `opencode.llm.first_output_ms`, `opencode.usage.cost_usd`, `session.id` |
| `LLM.stream` | llm | 2 | `SessionStep.attempt` | `gen_ai.provider.name`, `gen_ai.request.model`, `session.id`; no tokens |
| `Tool.execute` | tool | 1 | `SessionStep.attempt` | `gen_ai.tool.name`=read, `gen_ai.tool.call.id`, `gen_ai.agent.name`, `session.id`; status error (file not found, see § 7 item 10); no arguments, no result |
| 697 other rows | span | 697 | mixed | `effect` internals (`PluginHost.make` 94, `Plugin.load` 94, `FileSystem.readFileStringSafe` 53, `State.notify` 22, `Config.*`, `Provider.*`, `Git.*`, `http.server` GET/POST); no `gen_ai.*` or `ai.*` attributes |

- The binary's span names never start with `opencode.` (**run**: 0 rows). Its attribute names do (`opencode.llm.first_output_ms`, `opencode.usage.cost_usd`).

**v2.3.0 without the flag** (`oc230-noflag`, **run**): 698 spans, same 4 llm rows and the same tool row. The per-name inventory of the flag run differs only in the `Config.*` counts above.

**1.19.6 with the flag** (`m7d/runs/npm119-flag`, **run**): 1024 spans, 89 traces for one turn, scope `opencode`, service `opencode`. Kinds: span 1017, llm 3, chain 3, tool 1.

| Span | Kind | Count | Parent | Attributes (raw) |
|---|---|---|---|---|
| `ai.streamText.doStream` | llm | 3 | `ai.streamText` | `ai.model.id`, `ai.model.provider`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens` (131, 82, 131), `gen_ai.usage.output_tokens` (12, 17, 12), `ai.usage.*`, `ai.prompt.messages` (full messages, system prompt included), `ai.prompt.tools`, `ai.prompt.toolChoice`, `ai.response.text`, `ai.response.toolCalls`, `ai.response.finishReason`, `ai.request.headers.X-Session-Id`; events `ai.stream.firstChunk`, `ai.stream.finish` |
| `ai.streamText` | chain | 3 | `LLM.run` | `ai.prompt` (the same messages), `ai.response.text`, `ai.response.toolCalls`, `ai.usage.*` (totals), `session.id`, `operation.name`, `resource.name`; no `gen_ai.*` |
| `ai.toolCall` | tool | 1 | `ai.streamText.doStream` | `ai.toolCall.name`=read, `ai.toolCall.id`, `ai.toolCall.args` (the arguments, JSON); no result attribute; status error; event `exception` |
| `sql.execute` | span | 264 | mixed | `db.query.text` (SQL text, `INSERT … VALUES (?, ?)` parameterized; DDL and PRAGMA included; no bound values seen in 264 rows) |
| `FileSystem.*` 66, `Config.*` 56, `Session.*`, `AISDK.hook` 39, `Plugin.*`, `http.server` | span | rest | mixed | no `gen_ai.*` or `ai.*` attributes |

Keys with `gen_ai.*` on 1.x: 3 rows (the `ai.streamText.doStream` rows). Keys with `ai.*`: 7 rows.

**Without the flag** (`npm119-noflag`, **run**): 1024 spans, all kind `span`, 0 `ai.*` names. The flag adds the llm, chain and tool rows; the generic rows are unchanged (timing varies the counts by a few).

**Named `com.opencode` or `opencode.tool.*`**: absent from every run. Scopes are `effect` (v2) and `opencode` (1.x) (**run**, all 12 runs). LangWatch's matcher (`/home/sebastien/github/langwatch/modules/coding-agent/contract/src/telemetry/opencode.ts:3-17`: `matches: signalSays(signal, "opencode")`, `namePrefixes: ["opencode."]`, `OPENCODE_TOOL_SPAN = "opencode.tool."`; the file's comment says scope `com.opencode`) would match none of these rows.

LangWatch's comment "without this flag its OTLP exporter never exports a span" (`/home/sebastien/github/langwatch/sdks/typescript/src/cli/utils/opencode-config-flag.ts:2-3`) is contradicted for both builds in these runs: v2 without the flag posts 698 spans, and 1.x without the flag posts 1024 `span` rows (**run**).

**Scale.** One turn gives 31 traces (v2) and 89 traces (1.x). The 1.x run includes 264 `sql.execute` rows and 66 `FileSystem.*` rows.

### 10.4 Where the content is

- **v2.3.0 spans**: no prompt, completion, tool argument or tool result on any span. The raw attribute inventory has none (**run**: `raw-oc230-flag.txt`). Setting `OTEL_LOG_USER_PROMPTS=1 OTEL_LOG_TOOL_CONTENT=1 OTEL_LOG_ASSISTANT_RESPONSES=1 OTEL_LOG_TOOL_DETAILS=1` changes no attribute key (**run**: `oc230-flag-contentnames`). These names are in the v2 binary's strings (byte grep), but nothing reads them in the observed run.
- **v2.3.0 logs** (4 posts, `/v1/logs`): one record, body `cli starting`, carries the CLI argument array, which includes the user's prompt text (`"args":["run","--standalone","--auto","Read hello.txt and tell me what …"]`). No switch. (**run**: `m7d/runs/oc230-flag/recv/0002-logs.json`.)
- **1.19.6 spans, with the flag and no content variable set**: full content. `ai.streamText.doStream` and `ai.streamText` carry the complete system prompt and user message (`ai.prompt.messages`, `ai.prompt`), the tool definitions (`ai.prompt.tools`), and the completion (`ai.response.text`, `ai.response.toolCalls`). `ai.toolCall` carries the arguments (`ai.toolCall.args`, e.g. `{"filePath":"…/hello.txt","offset":0}`). The source of this default is the AI SDK's `experimental_telemetry`, which this run did not change (**run**: `npm119-flag/recv/0004-traces.json`, spans 291, 292, 444; the title-generation call at spans 320 and 321 of `0002-traces.json` carries its own prompt and completion).
- **1.19.6 logs** (2 posts): 31 records, no prompt or completion text (**run**).
- A switch that turns the 1.x content off was **not tested**. The only option tried was the flag itself. Open item (§ 10.8).

### 10.5 What cetra's normalizer makes of each run

Run with `m7d/norm-target/release/norm` (the existing binary), inventory from `m7d/inv.py`:

- **v2.3.0 flag run** (`oc230-flag`): 702 rows kept, 0 discarded. llm 4 (`SessionStep.attempt` x2 with tokens 82/17 and 131/12, provider openai, model gpt-4o-mini; `LLM.stream` x2 without tokens), tool 1 (`Tool.execute`, error), span 697. I/O present on none. Same counts as `oc230-noflag` plus the 4 config spans.
- **1.19.6 flag run** (`npm119-flag`): 1024 rows kept, 0 discarded (`norm.err`: `files=2 input_spans=1024 kept=1024 discarded=0`). llm 3 (`ai.streamText.doStream`: tokens 131/12 on model gpt-5.4-nano for the title call, and 82/17 and 131/12 on gpt-4o-mini; input and output present on all three), chain 3 (`ai.streamText`: input and output present, no tokens), tool 1 (`ai.toolCall`: input present, no output), span 1017.
- **1.19.6 no-flag run** (`npm119-noflag`): 1024 rows, all `span`, no tokens, no I/O.

### 10.6 Signals (run)

- **Logs off**: `OTEL_LOGS_EXPORTER=none` does not stop logs in either build. v2 posts 4 log requests (`oc230-flag-signals-off`, same as `oc230-flag`); 1.x posts 2 (`npm119-flag-signals-off`, same as `npm119-flag`).
- **Metrics off**: none of the 12 runs posts `/v1/metrics`, with or without `OTEL_METRICS_EXPORTER=otlp`. The builds send no metrics in these runs. (The plugin's metrics in § 5.1 came from the plugin.)
- **Protocol**: `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` is ignored. `oc230-flag-protobuf` and `npm119-flag-protobuf` send only `Content-Type: application/json` on `/v1/traces` and `/v1/logs`. Whether cetra's server accepts JSON OTLP on those paths was not checked here (§ 7 item 4).

### 10.7 Header form (run)

- Plain space: `oc230-flag` and `npm119-flag` receive `Bearer sk-fake...` (17 characters), decoded.
- `%20`: `oc230-flag-pct` and `npm119-flag-pct` receive `Bearer%20sk-fa...` (19 characters), literal. Both builds need the plain-space form.

### 10.8 Proposed keep and drop lists (facts only; the plan decides)

**v2.3.0**, keep rule `gen_ai.*` attribute present: 5 rows (`SessionStep.attempt` x2, `LLM.stream` x2, `Tool.execute` x1). The two `LLM.stream` rows duplicate the call of their parent and carry no tokens, so a token sum is not affected but a call count is. Candidate: keep `SessionStep.attempt` and `Tool.execute`, drop `LLM.stream`, drop the other 697 `span` rows. If a tree is wanted, `SessionRunner.runStep` is the parent of the model rows (run); it is not a GenAI row.

**1.19.6**, keep rule `ai.*` attribute present: 7 rows (`ai.streamText.doStream` x3, `ai.streamText` x3, `ai.toolCall` x1). Candidate: keep `ai.streamText.doStream` and `ai.toolCall`, drop `ai.streamText` (it duplicates the prompt and response of its doStream child and carries the usage totals a second time; the normalizer already reads the tokens from the doStream row). Drop `sql.execute` (264), `FileSystem.*`, `Config.*`, `Session.*`, `AISDK.hook`, `http.server *` and the other generic rows.

**Both**: the `com.opencode` scope and `opencode.*` span names are not present, so a matcher on them drops everything. A keep list must name the observed span names (above), not a prefix.

### 10.9 What left loopback

- Inside the namespace: no non-loopback connect. The 4 `connect()` calls per 1.x run go to `127.0.0.53` port 53 (the loopback address of the stub resolver; the namespace has only `lo`) (**run**: `m7d/runs/*/trace.strace`).
- Real home: one read-only stat of the worktree directory `/home/sebastien/travaux/llm-observer/.claude/worktrees/integrations` per run (`real-home.txt`). No write, create, rename or unlink under the real home (grep of `trace.strace`).
- Host, outside the namespace: `npm view opencode-ai time --json`, `npm view opencode-ai@1.19.6 dist.integrity dist.tarball`, and the same for `opencode-linux-x64@1.19.6` (registry.npmjs.org, metadata only; no tarball was downloaded). No GitHub download.
- Fake keys only (`sk-fake-m7`). The fake provider is `tools/server.py` on loopback. Nothing was built in the repo. Nothing was killed.

### 10.10 Open questions (not confirmed here)

1. **1.x content switch.** Is there any setting that stops `ai.prompt*`, `ai.response.*` and `ai.toolCall.args` on 1.19.6 (for example an AI SDK option the OpenCode build exposes, or the flag's own value)? Not tested. The 1.x source is not in E, only the binary.
2. **v2 content names.** The v2 binary contains `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_CONTENT`, `OTEL_LOG_ASSISTANT_RESPONSES` and `OTEL_LOG_TOOL_DETAILS` as strings, but the run showed no effect. Whether any of them is read in another code path was not checked. Not needed for the routes.
3. **Flag location.** Only the global `opencode.jsonc` was tested. v2 rejects the key by the schema (§ 10.2), so a project-level config would not change the result, but it was not run.
4. **Plugin vs binary for the `com.opencode` scope.** The § 5.1 claim stands: the `com.opencode` spans come from the community plugin (`oc230-plugin`), not from either build here.
5. **Logs of v2.** The `cli starting` record with the prompt text is in the logs, not in spans. Whether cetra stores log bodies, and so keeps this text, was not checked.
6. **Title call.** The 1.x run's first model call (`gpt-5.4-nano`, system prompt "You are a title generator") is a separate call the agent makes; whether it should count as a model call for the plan is open.
