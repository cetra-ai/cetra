# M7 spike (second round): OpenClaw through the gateway, and Omnigent

Status: spike, not a plan. Nothing here changes the design or the decisions; the open questions in § 7 need the user. Continues `m7-coding-agents.md` § 4 (OpenClaw) and § 7 (Omnigent) per D-IN-27 items 6 and 7.

Evidence root (E): `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7` (the first spike's root, reused; `m7-coding-agents.md` calls it E too). This round's evidence is under `E/m7b/`. Paths below are relative to E unless absolute. Each fact is tagged:

- **run**: observed by a command of this session, with the evidence path under `E/m7b/`.
- **docs**: read from the installed package's own docs or source (`E/pkgs/…`, `E/m7b/src/…`), or fetched from the vendor's docs site, with the path or URL.

## 1. Scope

- OpenClaw: run the built-in `diagnostics-otel` plugin through the gateway (the route competitors document), against the fake provider, with a fake key, one agent turn with a tool call; record span names, scopes, keys, what cetra's normalizer makes of them, the auth header form, and whether logs and metrics are also sent.
- Omnigent (omnigent.ai, PyPI `omnigent`): the same questions; if it needs an account or a hosted service, stop there and record what blocks it.

## 2. Versions

Cutoff: newest release published on or before 2026-11-17, with the first spike's convention (`--before 2026-11-17T23:59:59Z` / `--exclude-newer 2026-11-17T23:59:59Z`, which admits all of 2026-11-17 in UTC; the first spike's open question 3 about that reading is still open and applies to `@openclaw/diagnostics-otel` 2026.11.5, published 2026-11-17T07:48Z).

| What | Version | URL | Published (UTC) | sha256 |
|---|---|---|---|---|
| `openclaw` (npm) | 2026.11.5 | https://registry.npmjs.org/openclaw/-/openclaw-2026.11.5.tgz | 2026-11-17T09:59:33Z (**run**: `m7b/logs/npm-packument-openclaw.json`) | a57d1272b7d87bdbf2b0a0e18811dc1ca3ffc5de02229d1f9e2884bd38470794 (**run**: `m7b/dl/openclaw-2026.11.5.tgz`; byte-identical to the first spike's `tarballs/openclaw_2026.11.5.tgz`) |
| `@openclaw/diagnostics-otel` (npm) | 2026.11.5 | https://registry.npmjs.org/@openclaw/diagnostics-otel/-/diagnostics-otel-2026.11.5.tgz | 2026-11-17T07:48:43Z (**run**: `m7b/logs/npm-packument-diagnostics-otel.json`) | cdeb9dbe82d00185762d7352a59790daef89e8c420df3df2aec05d343f826c77 (**run**: `m7b/dl/diagnostics-otel-2026.11.5.tgz`; byte-identical to the first spike's `tarballs/openclaw-diagnostics-otel-2026.11.5.tgz`) |
| `omnigent` (PyPI) | 0.22.0 | https://files.pythonhosted.org/packages/c4/ee/8b32994cc36b2b7fe31a40cc45be4dded9d5d6dcd8d8c8a07ad4f1888c75/omnigent-0.22.0-py3-none-any.whl | 2026-11-13T04:11:57Z (**run**: `m7b/logs/pypi-omnigent-0.22.0.json`) | fd6ebc3b7b03bdf368526e763e46c36d9589b14a03cb7c8e60e58f1bd8d71840 (**run**: `m7b/dl/omnigent-0.22.0/omnigent-0.22.0-py3-none-any.whl`) |

Newer releases exist and were excluded: `@openclaw/diagnostics-otel` 2026.11.6 (2026-11-19T14:35Z), 2026.11.7 (2026-11-23T00:22Z), 2026.11.8 (2026-11-24T08:38Z); `omnigent` 0.23.0 (2026-11-19T02:50Z) (**run**: the packuments above).

The Omnigent run environment is a venv at `m7b/pkgs/omni-venv` (Python 3.12.3, 114 packages) built with `uv pip install --exclude-newer 2026-11-17T23:59:59Z 'omnigent[tracing,agents-sdk]==0.22.0'`; key pins: `omnigent-client==0.22.0`, `openai==2.44.0`, `openai-agents==0.13.6`, `claude-agent-sdk==0.2.183`, `fastapi==0.163.2`, `starlette==1.10.0`, `opentelemetry-sdk==1.45.2`, `opentelemetry-exporter-otlp-proto-http==1.45.2`, `opentelemetry-instrumentation-openai-agents-v2==0.1.0`, `opentelemetry-util-genai==1.3b0`. Full pin list: `m7b/logs/omni-venv-freeze.txt`; install log: `m7b/logs/install-omnigent.log` (**run**).

## 3. Method

Same harness as the first spike, reused:

- Every agent, gateway or CLI run executes inside `unshare -Urn` (loopback only) through `tools/netrun.sh` → `tools/inner.sh`; the fake provider and OTLP sink (`tools/server.py`) run on 127.0.0.1 inside the namespace; `strace` records `connect()` and file syscalls (**run**: `m7b/runs/<run>/trace.strace`, reduced after each run by `m7b/tools/post.sh` into `connects-ipv4.txt`, `connects-nonloopback.txt`, `connects-ipv6.txt` and `trace-meta.txt` with the original's size and sha256, because the raw files are 0.6–2.2 GB). The namespace has only the `lo` interface (brought up by `inner.sh`), so a non-loopback connect cannot succeed by construction; the strace summaries corroborate (only 127.0.0.1 and the 127.0.0.53 stub resolver appear). `omni-2` predates IPv6 capture: its reduced evidence records only the count of `sin6_addr` lines (56), not the addresses.
- One change to the shared fake this round: `tools/server.py` `pick_tool` now respects a declared `minimum` for numeric args and omits `limit`/`end_line` when the schema declares none (OpenClaw's `read` wants `offset >= 1`; omnigent's `sys_session_list` wants `limit` in 1..100; the old filler sent 0 for both and every tool call failed). **run**: the diff is in the file; runs started before the edit (`omni-1`, `omni-2`, `ocgw-1`, `ocgw-2`) used the old filler (their tool calls failed; see § 4.3, § 5.3).
- Runners: `m7b/runners/*.sh`; fake keys only (`sk-fake-m7`; gateway token `spike-gw-token`).
- Downloads (npm, PyPI) happened outside the namespace and are recorded in § 2 and § 8.
- Normalization uses cetra's real `normalize` through the first spike's harness (`norm-target/release/norm`, invoked by `tools/h.py`), writing `m7b/runs/<run>/out/<run>.norm.jsonl`.

## 4. OpenClaw

### 4.1 Why the first spike saw no spans

Two independent causes, both found this round:

1. **The plugin needs the host's internal diagnostics capability, granted only to trusted plugin installs.** The plugin's service does `const subscribe = ctx.internalDiagnostics?.onEvent; if (!subscribe) ctx.logger.error("diagnostics-otel: internal diagnostics capability unavailable")` (**docs**: `m7b/src/package/dist/.setup/runtime-api-iB_eY9Ao.mjs` — the plugin tarball extracted under `m7b/src/package/`). The host builds that capability only for `origin === "bundled" || trustedOfficialInstall === true`, and `trustedOfficialInstall` holds only for an npm-registry install (no `artifactKind`, no `sourcePath`, package name matching the official external plugin catalog) or an official-ClawHub install (**docs**: `pkgs/openclaw/node_modules/openclaw/dist/services-B34J0ZB6.mjs` `createPluginServiceDiagnostics`; `dist/managed-handoff-runtime.mjs` `resolvePluginTrust`, `isTrustedOfficialPluginInstallRecord`, `hasTrustedOfficialNpmProvenance`). The first spike installed from a local tarball (`npm-pack:…`), which the CLI flags as "outside ClawHub review and trust metadata" — such an install never gets the capability, in the gateway or out of it, and the failure is a single log line.
2. **The gateway itself had never started.** `openclaw gateway run` refuses a config without `gateway.mode`: "Gateway start blocked: existing config is missing gateway.mode … set gateway.mode=local manually" (**run**: first spike `runs/openclaw-8/gateway.log`). `gateway.mode` is `"local" | "remote"` (**docs**: `openclaw config schema`, captured at `m7b/runs/schema/config-schema.json`). With `gateway.mode` set but the plugin still installed from the local tarball, the gateway runs and still logs "internal diagnostics capability unavailable" — cause 1 on its own is enough to produce zero OTLP bodies (**run**: `m7b/runs/ocgw-1/gateway.log`; `http.jsonl` shows the two model calls and no OTLP request at all).

The first spike's `openclaw agent --local` runs hit cause 1. The docs say one-shot `--local` runs do export when the plugin is enabled and trusted, while `openclaw agent exec` never exports (**docs**: `pkgs/openclaw/node_modules/openclaw/docs/gateway/opentelemetry/setup.md` § "Which processes export").

### 4.2 How it was run this round

- Trusted plugin install, staged once outside the namespace (a recorded npm download): `openclaw plugins install --pin --accept-capabilities 'npm:@openclaw/diagnostics-otel@2026.11.5'` with `HOME` and the XDG dirs pointed at `m7b/stage` and `NODE_PATH` at `E/pkgs/openclaw/node_modules` (**run**: `m7b/stage/install.log`; `m7b/stage/install-connects.strace` shows connects only to registry.npmjs.org (104.16.0–11.34) and the 127.0.0.53 stub resolver, § 8). The staged `~/.openclaw` (project dir `npm/projects/openclaw-diagnostics-otel-67638bd2bf`, config, state db) is copied into each run's sandbox HOME. The installed plugin code is byte-identical to the tarball (`sha256 f8831b36…` for `dist/.setup/runtime-api-iB_eY9Ao.mjs` in both) (**run**).
- Gateway run 2 (`m7b/runners/oc-gw2.sh`, evidence `m7b/runs/ocgw-2/`): `openclaw.json` with `models.providers.fake` (baseUrl the fake, `api: openai-completions`, key `sk-fake-m7`), `gateway.mode: local`, `gateway.auth` token mode, `plugins.allow: ["diagnostics-otel"]` and `plugins.entries."diagnostics-otel".enabled: true`, and `diagnostics.otel` = `{ enabled, endpoint, protocol: "http/protobuf", serviceName: "openclaw", traces: true, metrics: true, logs: true, headers: { Authorization: "Bearer sk-fake-m7" }, captureContent: true, flushIntervalMs: 2000 }`. `openclaw gateway run --verbose` in the background (~95 s to `http server listening`, plugin load ~85 s), then `openclaw agent --timeout 150 -m "Read hello.txt and tell me what it says." --json`, then TERM to the gateway.
- Result: turn completed, 2 model calls to the fake (`/v1/chat/completions`, `User-Agent: OpenAI/JS 7.39.0`), 1 `read` tool call (failed: the old filler sent `offset: 0`), correct final text (**run**: `m7b/runs/ocgw-2/agent.json`, `http.jsonl`). The plugin logged "logs exporter enabled (OTLP/Protobuf)" and no capability error (**run**: `m7b/runs/ocgw-2/gateway.log`). OTLP bodies received: 6 traces, 28 metrics, 13 logs (**run**: `http.jsonl`).
- Gateway run 3 (`m7b/runners/oc-gw3.sh`, evidence `m7b/runs/ocgw-3/`): same, plus `agents.defaults.workspace` at the scratch repo and the fixed filler — the `read` tool call succeeded (`toolSummary.failures: 0`) (**run**: `m7b/runs/ocgw-3/agent.json`).
- One-shot local run (`m7b/runners/oc-local.sh`, evidence `m7b/runs/oc-local-1/`): trusted install, **no gateway**, `openclaw agent --local --json -m "Read hello.txt and tell me what it says."`. It works, and it is the cleanest result: the turn completed in ~4 s (span timestamps 2026-11-24T09:05:26→09:05:30Z), one 78 KB OTLP traces body flushed at process exit, 6 spans on one trace (`openclaw.harness.run` root → `openclaw.run` → `openclaw.context.assembled`, `openclaw.model.call` ×2 (82/17, 131/12), `openclaw.tool.execution` (`read`, succeeded, output the file text)), and — unlike the gateway — **no `openclaw.model.usage` aggregate and none of the gateway's phase/RPC noise** (**run**: `m7b/runs/oc-local-1/out/oc-local-1.norm.jsonl`, `http.jsonl`). With `metrics: false, logs: false` in `diagnostics.otel`, only `/v1/traces` was sent, same `Authorization: Bearer sk-fake…` header (**run**: `http.jsonl`).

### 4.3 What is exported, and what cetra's normalizer does with it

All spans are in one instrumentation scope: `openclaw` (h.py prints `openclaw@`), service `openclaw` (from `diagnostics.otel.serviceName`, configurable), resource `process.executable.name: openclaw-gateway` (**run**: `m7b/runs/ocgw-2/out/ocgw-2.dump.txt`). Trace shape: `openclaw.harness.run` (root) → `openclaw.run` → { `openclaw.context.assembled`, `openclaw.model.call` ×2, `openclaw.tool.execution` }; `openclaw.model.usage` is a separate parentless trace (**run**: parent ids in `out/ocgw-2.norm.jsonl`).

Span names observed in `m7b/runs/ocgw-2` + `ocgw-3` (37 spans kept each, none rejected) and `oc-local-1` (6 spans), and the normalizer's kind today. The gateway emits the whole table; the one-shot `--local` run emits only `harness.run`, `run`, `context.assembled`, `model.call`, `tool.execution` (no `model.usage`, no phase/RPC noise):

| Span | Count | Kind today | Notes |
|---|---|---|---|
| `openclaw.model.call` | 2 | `llm` | The real model calls. `gen_ai.operation.name` (`text_completion` on the openai-completions API), `gen_ai.request.model`, `gen_ai.system` (= the provider id, `fake` here; `gen_ai.provider.name` is only written under the `gen_ai_latest_experimental` opt-in), `gen_ai.usage.input_tokens`/`output_tokens`/`cache_read.input_tokens`/`cache_creation.input_tokens`, `openclaw.provider/model/api/transport`, `openclaw.model_call.request_bytes`/`response_bytes`/`time_to_first_byte_ms` (ms; 168/21 in ocgw-2, 215/29 in ocgw-3), prompt-size counters (`openclaw.model_call.prompt.*`). Tokens 82/17 and 131/12; provider normalizes to `fake`. Content: `input.value`/`output.value` (mime `application/json`, OpenClaw's `[{role, parts:[…]}]` JSON — the normalizer stores the parsed JSON as the span's input/output; the `parts` shape is not converted to cetra messages) plus duplicates under `openclaw.content.input_messages`/`output_messages`/`tool_definitions`. **run**: `out/ocgw-2.norm.jsonl`, `out/ocgw-3.norm.jsonl` |
| `openclaw.model.usage` | 1 (gateway only) | `llm` | Per-turn aggregate: `gen_ai.usage.*` 213/29 — exactly the sum of the two `model.call` spans. Separate trace, no parent. **Double-counts tokens with `model.call`.** Not emitted by the one-shot `--local` run. **run**: same files |
| `openclaw.run` | 1 | `span` | The agent turn. `openclaw.agent`, `openclaw.model`, `openclaw.provider`, `openclaw.outcome` (`completed`), `openclaw.trigger` (`user`); with `captureContent` also `input.value`/`output.value` (plain text). Parent of the `model.call`s and the tool span. **run**: same files |
| `openclaw.harness.run` | 1 | `span` | Root of the turn's trace; `openclaw.harness.id` (`openclaw`), `openclaw.harness.items.*`, provider/model/outcome. **run**: same files |
| `openclaw.tool.execution` | 1 (where the tool call succeeded: ocgw-3, oc-local-1) | `tool` | `gen_ai.tool.name` (`read`), `gen_ai.operation.name` (`execute_tool`), `gen_ai.tool.call.id`, `openclaw.toolName`, `openclaw.tool.source`, `openclaw.tool.params.kind`; content under `gen_ai.tool.call.arguments` (consumed by the normalizer into `input`) and a tool output key (into `output`), plus `openclaw.content.tool_input`/`tool_output`. No span was emitted for the *failed* call in run 2. **run**: `out/ocgw-3.norm.jsonl`, `out/oc-local-1.norm.jsonl` |
| `openclaw.context.assembled` | 1 | `span` | Prompt-assembly counters, no content. **run**: same files |
| `openclaw.gateway.rpc.dispatch`/`handler`/`response` | 12 | `span` | Gateway RPC plumbing noise. **run**: same files |
| `openclaw.diagnostic.phase` | 18–19 | `span` | Startup/phase timing noise (`openclaw.phase` = `gateway.ready`, `sidecars.*`, …). **run**: same files |

Also documented, not observed in these runs: `openclaw.skill.used`, `openclaw.exec`, `openclaw.webhook.processed`/`error`, `openclaw.message.processed` (with `input.value`/`output.value` under captureContent), `openclaw.message.delivery`, `openclaw.session.stuck`, `openclaw.tool.loop`, `openclaw.memory.pressure` (**docs**: `pkgs/openclaw/node_modules/openclaw/docs/gateway/opentelemetry/spans-and-events.md`). With `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`, request spans are named `{gen_ai.operation.name} {gen_ai.request.model}` and turn spans `invoke_agent`, both `CLIENT` kind (**docs**: same page) — not exercised.

**Auth header form received** (**run**: `m7b/runs/ocgw-2/http.jsonl`): every OTLP request (`/v1/traces`, `/v1/logs`, `/v1/metrics`) carries `Authorization: Bearer sk-fake…` (17 characters, decoded), exactly the value written in `diagnostics.otel.headers`; `Content-Type: application/x-protobuf`, `Transfer-Encoding: chunked`, `User-Agent: OTel-OTLP-Exporter-JavaScript/0.223.0`. Header key sent capitalized. No percent-decoding concern: the value comes from config JSON, not an env var.

**Logs and metrics: yes, all three signals.** `traces` and `metrics` default to on (unless `false`); `logs` is opt-in (`=== true`) with `logsExporter` `otlp` (default) | `stdout` | `both` (**docs**: plugin source, § 4.1 path). Metrics observed (35 names): `gen_ai.client.operation.duration`, `gen_ai.client.token.usage`, `openclaw.tokens`, `openclaw.model_call.*`, `openclaw.gateway.*`, `openclaw.queue.*`, `openclaw.run.duration_ms`, …; logs are gateway lifecycle records (INFO; bodies like "shutdown step … settled") (**run**: decoded from `m7b/runs/ocgw-2/recv/`).

Config surface confirmed in the plugin source (**docs**: `m7b/src/package/dist/.setup/runtime-api-iB_eY9Ao.mjs`): `diagnostics.enabled` (must not be `false`), `diagnostics.otel.enabled`, `.endpoint`, `.protocol` (only `http/protobuf`; resolved per signal from `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL`, then `OTEL_EXPORTER_OTLP_PROTOCOL`, then http/protobuf), `.headers` (object), `.serviceName` (fallback `OTEL_SERVICE_NAME`), `.traces`/`.metrics`/`.logs`/`.logsExporter`, `.sampleRate` (0–1, `ParentBasedSampler`+`TraceIdRatioBasedSampler`), `.flushIntervalMs`, `.captureContent` (`=== true`; gates `input.value`/`output.value`, tool content, log bodies, `openclaw.content.*`). This is the same surface the competitors' pages document (the task brief cites LangWatch's OpenClaw page; it was not reachable from here — langwatch.com is a domain-parking page and langwatch.ai/docs/openclaw returns 404 — so the config above is pinned to OpenClaw's own docs and source instead of guessing their URL).

### 4.4 Proposed verdict

**First-class** (proposed; the user decides). Both documented routes work offline against the fake provider, and the spans carry model, provider, tokens, TTFT and (with `captureContent`) content, in one stable scope, with a tool span. The **one-shot `--local` route is the better default** for the page and for `agents setup`: no gateway to run, a clean 6-span trace, no aggregate double-count, ~4 s per turn. The gateway route additionally works for an always-on setup but brings ~95 s of startup, 31–32 noise spans and the `model.usage` double-count (OC2). The remaining cost either way: the plugin must be installed from npm/ClawHub (a local-tarball install is silently non-exporting — a real trap for `cetra agents setup`), and `metrics` defaults to on so a spans-only setup must turn it off explicitly.

### 4.5 Proposed normalizer rules (gate: instrumentation scope `openclaw`)

Detection: scope name `openclaw` (single scope, all spans; `serviceName` is configurable so it is not the key). A `CodingAgent`-style variant is needed only if OpenClaw becomes first-class (D-CA-3/D-CA-4).

| # | Rule | Evidence |
|---|---|---|
| OC1 | `openclaw.model.call` → `llm` (already generic via `genai`); provider from `gen_ai.system` (OpenClaw writes `gen_ai.system` by default, not `gen_ai.provider.name`) | `m7b/runs/ocgw-2/out/ocgw-2.norm.jsonl` |
| OC2 | `openclaw.model.usage` → drop, counted in `discarded` (per-turn aggregate on gateway runs; 213/29 = 82/17 + 131/12, would double-count; absent from `--local` runs) | same |
| OC3 | `openclaw.run` → `agent` (input/output, `openclaw.agent`, outcome) | same |
| OC4 | `openclaw.harness.run` → `chain` (root wrapper) | same |
| OC5 | `openclaw.tool.execution` → `tool` (name from `gen_ai.tool.name`; already generic today) | `m7b/runs/ocgw-3/out/ocgw-3.norm.jsonl` |
| OC6 | Drop `openclaw.diagnostic.phase`, `openclaw.gateway.rpc.*`, `openclaw.context.assembled` (31–32 of 37 spans per gateway run; the `--local` run emits only `context.assembled` of these) | `m7b/runs/ocgw-2/out/ocgw-2.norm.jsonl` |
| OC7 | Read TTFT from `openclaw.model_call.time_to_first_byte_ms` (ms) on `openclaw.model.call` | same |

### 4.6 What `cetra agents setup openclaw` would write

- Run `openclaw plugins install @openclaw/diagnostics-otel` (npm or ClawHub; **never** a local tarball — § 4.1). This is more than settings-writing, so D-CA-10's "setup writes settings only" needs a decision: either setup shells out to the plugin install (network), or the page/install flow tells the user to run it.
- Merge into `~/.openclaw/openclaw.json`: `plugins.allow += "diagnostics-otel"`, `plugins.entries."diagnostics-otel".enabled = true`, `diagnostics.enabled = true`, `diagnostics.otel = { enabled: true, endpoint: <cetra base>, protocol: "http/protobuf", serviceName: "openclaw", traces: true, metrics: false, logs: false, headers: { Authorization: "Bearer <key>" } }` — `metrics: false` explicitly, or metrics arrive too.
- Tell the user to (re)start the gateway if they run one (`openclaw gateway restart`); one-shot `openclaw agent --local` turns pick the plugin up without a gateway (§ 4.2, `oc-local-1`). The page should document both routes, defaulting to `--local`.
- `remove`: undo the config merge; optionally `openclaw plugins uninstall diagnostics-otel`.

## 5. Omnigent

### 5.1 What it is, and the offline answer

- Omnigent (omnigent.ai) is an open-source (Apache-2.0) "meta-harness for building and running AI agents" — a layer over Claude Code, Codex, Pi and its own runtimes, with a terminal UI, a local server and optional cloud sandboxes (**docs**: omnigent.ai home page, fetched 2026-11-24; PyPI summary "declarative agent authoring and runtime framework"). The CLI entry points are `omni` and `omnigent` (**docs**: `m7b/src/omnigent-0.22.0/omnigent-0.22.0.dist-info/entry_points.txt`).
- **It runs locally with no account.** `omnigent run --harness openai-agents --model gpt-4o-mini -p "…"` with `OPENAI_BASE_URL`/`OPENAI_API_KEY` pointed at the fake provider spawns a local server on 127.0.0.1:6767 plus a runner and a harness subprocess, answers the prompt, and exits 0; no login step, no outbound call (**run**: `m7b/runs/omni-2/`, `omni-2.netrun.log`, strace summary in `connects-ipv4.txt`). `omnigent login` exists only for remote `--server` targets (**docs**: `omnigent/cli.py` `--server`/`login`). The `openai-agents` harness reads `OPENAI_BASE_URL`/`OPENAI_API_KEY` for its model client (**docs**: `omnigent/inner/openai_agents_sdk_executor.py` lines ~536–619).
- The `--no-session` flag is rejected for a harness launch (`--no-session requires an AGENT path`; **run**: `m7b/runs/omni-1.netrun.log`).
- Leftover processes: after the prompt is answered and the CLI exits 0, background children (server/host) keep running until the namespace is torn down; the spike's `timeout` kill shows as exit 137 in the wrapper (**run**: `m7b/runs/omni-2.netrun.log`). `omnigent stop` is the CLI's own command for this.
- Telemetry code (**docs**: `omnigent/runtime/telemetry.py`, `omnigent/inner/tracing.py` in the wheel): master opt-in `OMNIGENT_TELEMETRY_ENABLED` (off by default; when off, no provider, no spans), OTLP export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (`BatchSpanProcessor` + OTLP exporter, from the `tracing` extra), `OTEL_SERVICE_NAME` per component (`omni-server`, `omni-runner`, `omni-harness`, `omni-host`), content capture behind `OMNIGENT_OTEL_CAPTURE_CONTENT` (off by default; gates `input.value`/`output.value`/`policy.content`), `OMNIGENT_OTEL_HTTP_CLIENT_INSTRUMENTATION` (default on; off suppresses internal httpx spans), `OMNIGENT_OTEL_FASTAPI_INSTRUMENTATION` (default on when an OTLP endpoint is set), FastAPI/httpx/SQLAlchemy instrumentors wired by omnigent itself.

### 5.2 Runs

All inside the namespace, `OPENAI_BASE_URL=<fake>/v1`, `OPENAI_API_KEY=sk-fake-m7`, `OMNIGENT_TELEMETRY_ENABLED=true`, `OTEL_EXPORTER_OTLP_ENDPOINT=<fake>`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-fake-m7`, `OMNIGENT_OTEL_CAPTURE_CONTENT=true`, `OMNIGENT_ANALYTICS=0`, `DO_NOT_TRACK=1`:

- `omni-2` (documented route, `m7b/runners/omni-2.sh`): 452 spans kept (21 traces bodies; 10 metrics, 24 logs). `omni-3` (adds `OMNIGENT_OTEL_HTTP_CLIENT_INSTRUMENTATION=false`): 424 spans (21/12/22 bodies). `omni-4` (adds `OMNIGENT_OTEL_FASTAPI_INSTRUMENTATION=false`): 248 spans (10/12/23 bodies). Model calls went to `/v1/responses` (2 per run) (**run**: `http.jsonl` of each).

### 5.3 What is exported, and what cetra's normalizer does with it

Signals: **traces, logs and metrics are all sent** (omni-2: 21 traces, 10 metrics, 24 logs bodies) (**run**: `m7b/runs/omni-2/http.jsonl`). Auth: every OTLP request carries `authorization: Bearer sk-fake…` (17 characters, **decoded** — the OTel Python env parser decodes the `%20`; header key lowercase as sent) (**run**: same file). Metrics: `http.client.duration` (httpx), `omnigent.client.websocket.connections`; logs: server app warnings (scopes `omnigent.server.*`) (**run**: decoded from `recv/`).

The spans that matter (scope `omnigent`, service `omni-harness` / `omni-server` / `omni-host`):

| Span | Kind today | Notes |
|---|---|---|
| `agent:<name>` (`agent:openai-agents`) | `agent` | `openinference.span.kind=AGENT`, `gen_ai.operation.name=invoke_agent`, `gen_ai.agent.name`, `session.id`, and the **turn's aggregate usage** `gen_ai.usage.input_tokens/output_tokens/total_tokens` (213/29/242 — the sum of both model calls). `gen_ai.request.model` and `llm.model_name` are both `openai-agents` — the harness name, **not** `gpt-4o-mini` that served the calls. With capture on: `input.value`/`output.value` plain text. **run**: `m7b/runs/omni-3/out/omni-3.norm.jsonl` |
| `tool:<name>` (`tool:sys_session_list`) | `tool` | `openinference.span.kind=TOOL`, `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name`, `tool.name`, `session.id`, input/output JSON. In omni-2 the call failed (old filler sent `limit: 0`; omnigent wants 1..100) and the span still carried `error.message`; in omni-3/4 it succeeded. **run**: same file |
| `policy.evaluate` ×4 | `span` | Guardrail decisions (phases REQUEST/TOOL_CALL/TOOL_RESULT/RESPONSE), `policy.decision`, `session.id`; `policy.content` carries the prompt/tool args/response JSON **only under content capture** (**docs**: `omnigent/runtime/policies/engine.py` `evaluate`). |
| `host.launch_runner` / `host.stat` / `host.stop_runner` | `span` | Scope `omnigent.frames`, service `omni-host`. |
| FastAPI/httpx internal spans | `span` | The bulk: omni-2 = 443 of 452 spans are `opentelemetry.instrumentation.fastapi` request/`http send`/`http receive`/websocket spans from omnigent's own instrumentation of its server/runner/harness APIs, plus httpx client spans. With the http-client flag off (omni-3) the model calls' httpx spans disappear too. With both flags off (omni-4), FastAPI **0.163.2's own native telemetry** (`fastapi.endpoint`/`fastapi.dependencies`/`fastapi.serialization`, scope `fastapi` — **docs**: `m7b/pkgs/omni-venv/lib/python3.12/site-packages/fastapi/telemetry/`) still emits 226 spans plus 13 httpx spans (239 of 248), ungated by omnigent's flags. |

**No per-call LLM spans on the documented route.** The model calls appear only as httpx `POST` spans with legacy `http.method`/`http.url` keys (no `gen_ai.*`), which the documented optional flag suppresses (**run**: `omni-2` vs `omni-3`). The bridge that would emit GenAI llm spans — `opentelemetry-instrumentation-openai-agents-v2` (installed by the `agents-sdk` extra, entry point `openai_agents`) — is **never wired by omnigent**: no reference to it exists anywhere in the wheel (searched; only FastAPI/httpx/SQLAlchemy instrumentors are called, `omnigent/runtime/telemetry.py` lines ~438–531), and the openai-agents SDK 0.13.6 has no OTel bridge of its own (**docs**: `agents/tracing/` has no opentelemetry reference). cetra's normalizer therefore sees zero `llm` spans from Omnigent; the only tokens are the aggregate on the agent span.

Isolation note: the omnigent processes read two paths under the real home (`/home/sebastien/.claude-extra/settings.json`, `/home/sebastien/.local/lib/python3.10`) — reads only, presumably the claude-agent-sdk probing for a Claude login via the passwd entry (**run**: `m7b/runs/omni-2/real-home.txt`).

### 5.4 Proposed verdict

**Captured page** (proposed; the user decides): the route is real and offline (no account), spans arrive and normalize into agent/tool kinds with a session id, and the page can pin the exact env-var set (`OMNIGENT_TELEMETRY_ENABLED`, `OTEL_EXPORTER_OTLP_*`, capture flag, both instrumentation-off flags). Not first-class: no per-call llm spans (the one bridge that exists is not wired by the product), the only tokens are the turn aggregate on the agent span, `gen_ai.request.model` carries the harness name rather than the model, and the internal-API span flood needs FastAPI-native telemetry disabled in code, not env. No `cetra agents setup` target: the route is environment variables on a framework the user already launches; in the source, `telemetry.init` reads only `os.environ` (**docs**: `omnigent/runtime/telemetry.py`), so there is no OTLP settings file to merge (`~/.omnigent/config.yaml` holds the usage-analytics opt-out, a separate mechanism — **docs**: omnigent.ai `/docs/deploy/telemetry`).

### 5.5 Proposed normalizer rules, if the user wants more than the page

| # | Rule | Evidence |
|---|---|---|
| OM1 | Scope `omnigent`: `agent:<name>` → `agent` (read the aggregate `gen_ai.usage.*` as the turn's tokens — they are the only usage anywhere); `tool:<name>` → `tool` (already generic); `policy.evaluate` and scope `omnigent.frames` → drop (guardrail/host noise) or keep `policy.evaluate` as `span` | `m7b/runs/omni-3/out/omni-3.norm.jsonl` |
| OM2 | Services `omni-server`/`omni-runner`/`omni-host`/`omni-harness`: drop scopes `opentelemetry.instrumentation.fastapi`, `opentelemetry.instrumentation.httpx` and `fastapi` (internal plumbing; 95 % of spans) — or do not, and rely on the page telling users to set both `OMNIGENT_OTEL_*_INSTRUMENTATION=false` (which still leaves FastAPI-native spans) | `omni-2`/`omni-3`/`omni-4` norm.jsonl |
| OM3 | If `opentelemetry-instrumentation-openai-agents-v2` ever gets wired (by omnigent or by user code), its llm spans arrive under GenAI semconv and need no rule — re-run this spike's harness then | § 5.3 |

## 6. Network: what left loopback

**This session — downloads and docs reads only, all recorded:**

- PyPI metadata (pypi.org, JSON API): `omnigent`, `omnigent-client`, `opentelemetry-instrumentation-openai-agents-v2` (`m7b/logs/pypi-*.json`); wheel download from files.pythonhosted.org (§ 2).
- uv install of `omnigent[tracing,agents-sdk]==0.22.0` + 113 dependencies from pypi.org/files.pythonhosted.org (`m7b/logs/install-omnigent.log`, `m7b/logs/omni-venv-freeze.txt`).
- npm packuments (registry.npmjs.org): `openclaw`, `@openclaw/diagnostics-otel` (`m7b/logs/npm-packument-*.json`); tarball downloads from registry.npmjs.org (§ 2).
- The staged plugin install outside the namespace: `openclaw plugins install --pin npm:@openclaw/diagnostics-otel@2026.11.5` — connects only to registry.npmjs.org (104.16.0–11.34, verified against `getent ahostsv4 registry.npmjs.org`) and the 127.0.0.53 stub resolver (**run**: `m7b/stage/install-connects.strace`). No clawhub.ai contact (its IP 216.150.1.1 does not appear).
- Docs pages fetched with WebFetch: omnigent.ai `/`, `/docs/use/coding-agents`, `/docs/deploy/telemetry` (readable); mlflow.org/docs/latest/tracing/integrations/omnigent/ (403), raw.githubusercontent.com mlflow docs path (404), langwatch.com/docs/openclaw (domain-parking page), langwatch.ai/docs/openclaw (404). No product binary was downloaded from any of them.

**Everything else stayed on loopback.** Every agent/gateway/CLI run was inside `unshare -Urn` (only `lo`); the per-run strace reductions (`connects-ipv4.txt`, `connects-nonloopback.txt`, `connects-ipv6.txt`) show only 127.0.0.1 and 127.0.0.53, with empty non-loopback files, for `ocgw-1`, `ocgw-2`, `ocgw-3`, `oc-local-1`, `omni-2`, `omni-3`, `omni-4` (**run**: those files). `omni-2`'s IPv6 connect addresses were not captured (56 `sin6_addr` lines, count only; § 3); `omni-3` and `omni-4`, captured with the fixed reducer, show no IPv6 connects at all; the namespace makes non-loopback unreachable regardless. The help/schema probe runs (`help`, `schema`, `phelp`) made no IPv4 connects at all.

## 7. Open questions (need the user)

1. **OpenClaw verdict**: first-class as proposed in § 4.4 (with the plugin-install step in `agents setup` — D-CA-10 says settings only), or captured page? The plugin-install trust trap (§ 4.1) is the main operational cost; the `--local` route keeps the setup story simple (§ 4.2).
2. **OpenClaw noise rules**: OC6 drops `openclaw.diagnostic.phase`, `openclaw.gateway.rpc.*` and `openclaw.context.assembled` — confirm, or keep some as `span`?
3. **Omnigent verdict**: captured page as proposed in § 5.4, or setup-only page (env vars, no fixture), or nothing until omnigent wires the agents-sdk bridge (OM3)?
4. **Omnigent model attribute**: `gen_ai.request.model` = the harness name (`openai-agents`), and the only tokens are the turn aggregate on the agent span (§ 5.3). If Omnigent gets rules, is an agent-span-only token source acceptable, given every other first-class agent has per-call llm spans?
5. **Cutoff reading** (carried over from the first spike's open question 3): `@openclaw/diagnostics-otel` 2026.11.5 is published 2026-11-17T07:48Z. Under "before 2026-11-17T00:00Z" it is ineligible and both OpenClaw pieces would need re-picking and a re-run.

## 8. Corrections to the first spike

- The first spike's OpenClaw verdict "not supported as tested (0 OTLP bodies in 8 runs)" is superseded: the gateway route exports once the plugin install is trusted and `gateway.mode` is set (§ 4).
- The first spike's `runs/openclaw-7` failure ("gateway agent requires credentials") and `openclaw-8` ("does not recognize option --password") were both symptoms of a gateway that never started (missing `gateway.mode`); with the gateway up, `openclaw agent` authenticates from `gateway.auth` in the shared config and needs no CLI flag (**run**: `m7b/runs/ocgw-2/agent.json`, status ok).
- The first spike's open question 1 ("Omnigent: which package, product or URL?") is answered: PyPI `omnigent` 0.22.0, omnigent.ai, console scripts `omni`/`omnigent` (§ 5.1).
- The first spike could not verify the plugin's release date; this round records it from the npm packument (§ 2).
