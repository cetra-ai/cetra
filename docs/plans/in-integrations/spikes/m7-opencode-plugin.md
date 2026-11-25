# M7 spike: the cetra OpenCode plugin

Status: spike, not a plan. It answers what `cetra agents setup opencode` needs before a plan is written: which OpenCode build users run, what each build's plugin API is, what Laminar's plugin does, and whether a cetra plugin can produce clean spans with OpenCode's own exporter silent. Nothing in the product changed. The prototype is `docs/plans/in-integrations/spikes/m7-files/cetra-opencode-prototype.ts`. Nothing is committed.

Evidence roots:

- **E** = `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m7`. The previous spike's tools (`E/tools/netrun.sh`, `E/tools/server.py`, `E/tools/h.py` with the real cetra normalizer), the OpenCode v1 install (`E/pkgs/opencode`) and the v2.3.0 binary (`E/m7c/bin/oc-2.3.0/opencode`).
- **X** = this spike's runs, `E/m7e/runs/<name>/` (run log, `recv/` OTLP bodies, `http.jsonl`, `trace.strace`, `out/<name>.norm.jsonl`). Runner: `E/m7e/runners/oc.sh`. Prototype source: `E/m7e/proto/cetra-opencode.ts` (same bytes as the repo copy).
- **T** = npm tarballs, `E/m7e/tarballs/`. Extracted sources, `E/m7e/x/`.

Tags: **run** = observed, with the evidence path; **docs** = read from a package, a source file (`file:line`) or a URL.

## 1. Versions

Rule: the newest release published at least 7 days before 2026-11-24 10:00 UTC, so on or before 2026-11-17 10:00 UTC.

| What | Version | Published (UTC) | sha256 | Evidence |
|---|---|---|---|---|
| OpenCode v1 wrapper (npm `opencode-ai`, dist-tag `latest`) | **1.19.6** | 2026-11-17T07:22:20.263Z | tarball `ef5c2a900b41cdcddcd0009a535c8a9f34e536f91240d0156be4e2d0b0287d13` (3 045 B) | **run**: `npm view opencode-ai dist-tags.latest` = 1.19.6; `T/opencode-ai/opencode-ai-1.19.6.tgz` |
| OpenCode v1 binary (npm `opencode-linux-x64` 1.19.6, the one the runs used) | 1.19.6 | same release | `aab0a58cd7fc5a1f17495b76c08e79989d331653d8b63156aca2f6e9e36d1173` | **run**: `E/pkgs/opencode/node_modules/opencode-linux-x64/bin/opencode`; `opencode --version` prints `1.19.6` (`X/v1-final/run.log`) |
| `@opencode-ai/sdk` (v1 event types) | 1.19.6 | 2026-11-17T07:22:10.408Z | `ab512280ef184e602bc9ebabafd6eea60ccecfd682c35e4a279af6ab2242cd18` (101 209 B) | **run**: `T/sdk/*.tgz`, extracted to `E/m7e/x/sdk/package` |
| `@opencode-ai/plugin` (v1 plugin types) | 1.19.6 | 2026-11-17T07:22:37.580Z | `0b830d7d1568362fe4e8fbd7598872655373d24b688ee1b80f1569f585da5e17` (11 985 B) | **run**: `T/opencode-plugin/`, extracted to `E/m7e/x/ocp/package` |
| OpenCode v2 CLI (npm `@opencode/cli`) | **2.3.0** (the v2 line's pin) | 2026-11-13T08:54:17.847Z | npm integrity `sha512-Q5RV8jLJ4O8bY2O8X2hkBOkgPf6+nDB9MEkz/Q/Cc4Ojd2SwUFFzEd1LzgNGKiXiS7g/9+/X4eqhrxY60fLMOQ==` | **run**: `npm view @opencode/cli@2.3.0`; GitHub release, see the setup-routes spike § 2 |
| OpenCode v2.3.0 binary (the one the runs used) | 2.3.0 | GitHub release 2026-11-13T08:55:07Z | binary `2e0e4656521ca56e5ba16a00d48429958b8105b3fa6466710a4350e68759381f`; release tarball `fc0f81522aceeabac47693d88389efa2bc517ce74c2c38fd6967819fcc6e4d3b` | **run**: `opencode --version` prints `opencode v2.3.0` (`X/v2-final/run.log`) |
| Laminar plugin (npm `@lmnr-ai/opencode-plugin`, dist-tag `latest`) | 0.2.0 | 2026-10-12T15:12:24.826Z | tarball `93081821c3f6d9ef646a54cc0c0c9466b7cc189087cc2d34aae6c7138eb74943` (45 765 B) | **run**: `npm view`; `T/lmnr-plugin/`, extracted to `E/m7e/x/lmnr/package` |

Ineligible, for the record: `@opencode/cli` 2.3.1 (2026-11-18), 2.3.2, 2.4.0 (2026-11-23), **2.4.1** (2026-11-23T15:56:19.960Z, which is npm's `latest` today). No 1.x release after 1.19.6: the `opencode-ai` time map lists only `0.0.0-dev-*` snapshots after 2026-11-17 (**run**: `npm view opencode-ai time --json`). `@opencode-ai/cli` on npm carries stale `0.0.0-beta-*` tags from 2026-09 and is not the v2 package (**run**: `npm view @opencode-ai/cli dist-tags`). `npm view opencode` returns 404.

Downloads recorded: the npm registry (`registry.npmjs.org`) for the four tarballs and the metadata above. Nothing else was downloaded; the v1 and v2 binaries were installed by the previous spike.

## 2. Method

- OpenCode runs happen inside `unshare -Urn` through `E/tools/netrun.sh` (loopback only). The fake provider and OTLP sink are `E/tools/server.py`, which answers the OpenAI Responses API (`/v1/responses`) with one tool call (`read`, 82/17 tokens) and then text (131/12). Fake key `sk-fake-m7`. Every run used a fresh work directory with `hello.txt`.
- The prototype is installed where `cetra agents setup opencode` would write it: the global config directory, `$XDG_CONFIG_HOME/opencode/plugins/cetra-opencode.ts` (`XDG_CONFIG_HOME` points into the sandbox). The project `opencode.json` only sets the model and the fake provider; no plugin entry is added. `OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_DISABLE_MODELS_FETCH`, `OPENCODE_DISABLE_LSP_DOWNLOAD` and `OPENCODE_DISABLE_DEFAULT_PLUGINS` are set in every run.
- Each run's OTLP bodies were normalized with cetra's real normalizer (`E/tools/h.py`, `norm-target/release/norm`), and each kind, token count and parent link was read from the normalized output.
- Commands: v1 `opencode run --auto "Read hello.txt and tell me what it says."`; v2 `opencode run --standalone --auto "..."`. Both exit 0 and answer the read (`X/*/run.log`).

## 3. Which build users run

**Answer: npm `latest` is v1 (1.19.6). The official v2 installer is the v2 line. The two lines come from one repository and are both current on the same product, but releases have moved to v2.**

- `npm install -g opencode-ai` installs **1.19.6**. This is npm's `latest` tag today, and the wrapper's `optionalDependencies` pin the platform binaries to 1.19.6 (**run**: `T/opencode-ai/package/package.json`; **docs**: the README, which the registry hosts).
- `https://opencode.ai/install` answers with a 307 redirect to `raw.githubusercontent.com/anomalyco/opencode/refs/heads/v1/install` (**docs**: WebFetch of `https://opencode.ai/install`, 2026-11-24). That is the **v1** branch's install script. The script itself was not fetched: the redirect target is a raw GitHub file, outside this spike's download sources.
- `https://opencode.ai/v2/install` runs the v2 installer. It downloads a `<scope>/cli-<target>` package whose version comes from a metadata endpoint, and falls back to `@opencode-ai/cli-<target>` (**docs**: WebFetch of `https://opencode.ai/v2/install`). The page does not say which version the endpoint returns today. Today's npm latest for the v2 package is 2.4.1, which is ineligible under the 7-day rule, so this spike pins 2.3.0.
- v1 or v2, same product? The repository is `anomalyco/opencode`. Its `main` tree is v2 (`packages/cli/package.json` name `@opencode/cli`, version 2.3.0; **run**: `E/m7c/src/opencode-src`), and the v1 install script lives on branch `v1` (redirect above). The Laminar README supports both ("OpenCode V2 ... OpenCode V1 (1.4 and newer)", **docs**: `T/lmnr-plugin/package/README.md`). The config file (`$schema` `https://opencode.ai/config.json`) and the OTEL variables are shared. The v2 page does not say what the v2 line replaces.
- So: v2 is the successor in the sense that releases moved there: 2.3.0 to 2.4.1 between 2026-11-13 and 2026-11-23, against no 1.x release after 2026-11-17. v1 is still on npm `latest`, so many users are on it. The plugin must target both. Whether v1 is deprecated is not stated by any source read here.

## 4. Plugin API, in both builds

### 4.1 Loading

| | v1 (1.19.6) | v2 (2.3.0) |
|---|---|---|
| Discovery | `.opencode/plugins/` in each project directory and `~/.config/opencode/plugins/` globally (**docs**: `opencode.ai/docs/plugins`, quoted "Global plugins use the same discovery layout under the OpenCode config directory") | `plugin/` and `plugins/` directories under each config root, global or project (**docs**: `E/m7c/src/opencode-src/packages/core/src/plugin/source-directory.ts:7`; `packages/core/src/config/plugin/source.ts:125-133`) |
| Config key for npm packages | `plugin` array (**docs**: `E/m7e/x/ocp/package/dist/index.d.ts:49`; Laminar README). The opencode.ai docs page says `plugins` | `plugins` array, `info.plugins` (**run/source**: `packages/core/src/config/plugin/source.ts:140`); object form `{ package, options }` |
| Default export | `{ id?, server: Plugin, tui? }` (`index.d.ts:52`, `PluginModule`), `Plugin = (input, options) => Promise<Hooks>` (`index.d.ts:51`) | `{ id, effect }` or `{ id, setup }` (`packages/core/src/plugin/module.ts:180`; `packages/plugin/src/promise/plugin.ts:64`) |
| Global file, no config entry | **run**: `X/v1-final` loaded the global file (spans from `cetra-opencode`, no plugin key) | **run**: `X/v2-final/xdg/data/opencode/log/opencode.log`: `loading plugin ... xdg/config/opencode/plugins/cetra-opencode.ts` |

The prototype's default export is `{ id, setup, server }` (the same dual shape the Laminar package uses, `T/lmnr-plugin/package/dist/index.mjs`, `plugin` object). One file therefore loads in both builds, and each build calls only its own half. **run**: both `v1-final` and `v2-final` produce 4 spans from it.

### 4.2 Hooks and events

| Need | v2 (`@opencode/plugin` 2.3.0, source `packages/plugin/src`) | v1 (`@opencode-ai/plugin` 1.19.6, `index.d.ts`) |
|---|---|---|
| Session start | event `session.created` (data: `sessionID`, `parentID?`, `agent?`, `model?`; `client/src/promise/generated/types.ts:2379`) | event `session.created` (`@opencode-ai/sdk` `types.gen.d.ts:494`) |
| Session end | event `session.deleted` (`types.ts:1264`) | event `session.deleted` (`types.gen.d.ts:506`) |
| Turn start | event `session.execution.started` (`types.ts:1322`); prompt hook `session.hook("prompt")` (`packages/plugin/src/effect/session.ts:201`) | hook `chat.message` (`index.d.ts:187`) |
| Turn end | `session.execution.succeeded` / `.failed` / `.interrupted` (`types.ts:1332`, `1342`) | event `session.idle` (`types.gen.d.ts:414`) |
| Request before a model call | hook `session.hook("context")`: `system`, `messages`, `tools`, `options` (`effect/session.ts:204`, `SessionContext`) | hook `experimental.chat.messages.transform` (`index.d.ts:259`): no session id, so not attributable; `chat.params` (`index.d.ts:203`) gives the model, not the messages |
| Model call start | event `session.step.started` (`agent`, `model {providerID, id}`, `started`; `types.ts:1412`) | none. `message.updated` (assistant) arrives when the message is created (`types.gen.d.ts:130`) |
| Model call end | `session.step.ended` / `.failed` (`tokens {input, output, reasoning, cache {read, write}}`, `finish`, `cost`; `types.ts:1453`) | `message.updated` with `info.time.completed`, `info.tokens`, `info.cost`, `info.modelID`, `info.providerID`, `info.finish`, `info.error` (`types.gen.d.ts` `AssistantMessage`) |
| Response text | `session.text.ended` (`assistantMessageID`, `text`; `types.ts:1504`) | event `message.part.updated`, part `type: "text"` |
| Tool call start | `session.tool.input.started` (`id`, `name`), then `session.tool.called` (`id`, `input`, `assistantMessageID`; `types.ts:1566`) | hook `tool.execute.before` (`index.d.ts:235`): `input {tool, sessionID, callID}`, `output.args` |
| Tool call end | `session.tool.success` (`content`) / `session.tool.failed` (`error`; `types.ts:2150`, `2168`) | hook `tool.execute.after` (`index.d.ts:249`): `output.output`; an error only as part state `status: "error"` in `message.part.updated` (`ToolPart`, `ToolStateError`) |
| Tool API in v2 | `ToolHooks` `execute.before` / `execute.after` (`packages/plugin/src/effect/tool.ts:20-29`) also exist; not used | — |
| Session id | `sessionID` on every event and hook input | `sessionID` on hook inputs and `properties.sessionID` / `info.sessionID` |
| Token shape | `TokenUsageInfo` (`types.ts:60`) | `AssistantMessage.tokens` (same fields) |

**Is the API the same in 1.x and v2? No.** v1 is a flat `Hooks` object with an `event` bus hook and per-call hooks. v2 is an Effect-style `Plugin` with a `setup(context)` and a domain per concern (`session`, `event`, `tool`, `aisdk`, ...). Both expose the same ids and the same token fields, but v1 has no request-start event and no per-session request hook. `@opencode-ai/plugin` 1.19.6 also ships a `dist/v2/` tree (`dist/v2/effect/plugin.d.ts`: `define` and `Plugin` with `effect(context)`). Its `Context` (`dist/v2/effect/context.d.ts`) has only the agent, aisdk, catalog, command, integration, plugin, reference and skill domains: no session and no event domain. The v2 session and event domains exist in the `@opencode/plugin` 2.3.0 source only.

Laminar's README says a v2 plugin reads the event stream and the `context` hook (**docs**: `T/lmnr-plugin/package/README.md`, "How it works"). That matches the table.

## 5. Laminar's plugin (`@lmnr-ai/opencode-plugin` 0.2.0)

Read from its published package (`T/lmnr-plugin/`), not copied.

- Export: `{ id: "laminar", setup: setupV2, server: serverV1 }` (`dist/index.mjs`, the last block). The same dual shape as the prototype.
- **v2 path** (`setupV2`): `ctx.session.hook("prompt" | "context" | "compaction")`, `ctx.event.subscribe(...)`, one `opencode turn` span per execution, `ai.llm <provider>:<model>` spans per `session.step.*`, a tool span per `session.tool.called`, usage from `session.step.ended.data.tokens`. Its usage code adds cache read and write to `input` (`usageAttributes`), so `TokenUsageInfo.input` excludes cache in v2. Sent through the Laminar SDK (gRPC/HTTP), not OTLP.
- **v1 path** (`serverV1`): its `config` hook **turns on `experimental.openTelemetry`** and builds spans from OpenCode's built-in AI-SDK telemetry plus a Laminar span processor, keyed by session id. Its `event` hook flushes on `session.idle`.
- Its frontend parser (`/home/sebastien/github/lmnr/frontend/lib/spans/types/opencode.ts`, lines 1-70) expects the v2 `LLM.Message` shape: `role` plus `content` parts of type `text`, `reasoning`, `tool-call` (`id`, `name`, `input`), `tool-result`, `media`. The prototype sends the v2 `context` messages as-is, and the normalizer parsed them (`X/v2-final` input).
- Laminar reads `LMNR_PROJECT_API_KEY` from the environment or from `.env` files (**docs**: README). It does nothing without a key.

## 6. Prototype and runs

The prototype (`cetra-opencode-prototype.ts`, 282 lines, no imports) sends OTLP/HTTP JSON with `fetch`, one flush per turn end, scope `cetra-opencode`. GenAI attributes follow the pi extension. It reads its endpoint from `CETRA_OTLP_ENDPOINT` and `CETRA_OTLP_HEADERS`, not from `OTEL_EXPORTER_OTLP_*` (see § 6.2 for why). Content capture is gated by `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT`, as in pi.

### 6.1 Normalized result (final runs)

`X/v2-final/out/v2-final.norm.jsonl` and `X/v1-final/out/v1-final.norm.jsonl` (content on, one turn, one tool call):

| Span | kind | provider | model | tokens in / out | status | I/O |
|---|---|---|---|---|---|---|
| `invoke_agent` | agent (root) | — | — | — | ok | — |
| `chat gpt-4o-mini` (1st) | llm | openai | gpt-4o-mini | 82 / 17 | ok | v2: input = system + user, output = tool call `read` with arguments, finish `tool-calls`. v1: no input (§ 8), output: tool call not captured |
| `execute_tool read` | tool | — | — | — | ok | input `{"path": ...}` (v2) or `{"filePath": ...}` (v1, the same tool under a different schema); output: the file read |
| `chat gpt-4o-mini` (2nd) | llm | openai | gpt-4o-mini | 131 / 12 | ok | v2: input = full conversation; output = the answer text. v1: output = the answer text |

Parentage is consistent in both: every chat and tool span's parent is the `invoke_agent` span id; the root has no parent. Both builds give the same kinds, tokens, provider and model. The first run set (`X/v2-content`, `X/v1-content`) matches these numbers; they were taken before the cache fix in § 6.3.

### 6.2 OpenCode's own exporter: silent or noisy?

Same run, with the standard OTEL variables also set (`BUILTIN=1` in the runner, `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`, no experimental flag):

| Run | OTLP trace bodies | Spans by scope |
|---|---|---|
| v2 with the plugin, `CETRA_*` only (`X/v2-content`) | 1 | `cetra-opencode` 4 |
| v2 with the plugin **and** `OTEL_EXPORTER_OTLP_ENDPOINT` (`X/v2-builtin`) | 4 | `effect` 700 + `cetra-opencode` 4 |
| v1 with the plugin, `CETRA_*` only (`X/v1-content`) | 1 | `cetra-opencode` 4 |
| v1 with the plugin **and** `OTEL_EXPORTER_OTLP_ENDPOINT` (`X/v1-builtin`) | 4 | `opencode` 638 + `cetra-opencode` 4 |

**Answer: OpenCode's built-in exporter cannot be turned off while the standard endpoint is set.** In v2.3.0 the binary exports its own traces and logs as soon as `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and no switch short of unsetting the endpoint turns the traces off (`E/m7c/runs/oc230-noplugin`, setup-routes § 5.1). In v1 the same happens without the experimental flag, which the previous spike did not show: 638 spans, scope `opencode`, in `X/v1-builtin`. So the plugin must not read `OTEL_EXPORTER_OTLP_*`. With its own variable names, neither build sends noise (rows 1 and 3). The plugin still reads `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT`, which triggers no export on its own (rows 1 and 3 have it set in the content runs).

### 6.3 Cache tokens (a fix found during the spike)

`gen_ai.usage.input_tokens` is cache-inclusive in cetra (`crates/otel/src/usage.rs`, "gen_ai.usage.* stays inclusive"). The first draft emitted OpenCode's `input` raw, which is wrong whenever cache is present. The fake provider reports no cache, so no run showed the error. The final prototype sets `input_tokens = input + cache.read + cache.write` in both builds, and keeps the raw cache counts as separate attributes. Whether OpenCode's `input` excludes cache for every provider is not verified (§ 8).

### 6.4 Failure cases

| Run | Plugin | Result |
|---|---|---|
| `X/broken-v2-syntax` | syntax error | exit 0, read answered, no spans; log `WARN failed to load plugin` |
| `X/broken-v2-throw` | throws at load | exit 0, read answered, no spans; log `WARN failed to load plugin` (two lines) |
| `X/broken-v1-syntax` | syntax error | exit 0, read answered, no spans; no log line about the plugin |
| `X/broken-v1-throw` | throws at load | exit 0, read answered, no spans; no log line about the plugin |

**A broken plugin is not fatal in either build.** v2 logs a warning, v1 logs nothing that names the file. Setup cannot rely on OpenCode to report a broken cetra plugin; it must check the file itself or give a command that does.

## 7. Setup facts

- **Path.** `$XDG_CONFIG_HOME/opencode/plugins/cetra-opencode.ts`, or `~/.config/opencode/plugins/cetra-opencode.ts` when `XDG_CONFIG_HOME` is unset (**docs**: opencode.ai plugins page; **run**: both builds load the file from the XDG path, `X/v1-final`, `X/v2-final/xdg/data/opencode/log/opencode.log`). The file name is the prototype's; the plan decides the name and a marker line, as for pi.
- **Config edit.** None. The runs use the global file with a project `opencode.json` that only sets the model and provider, and both builds load the plugin (run evidence above).
- **Endpoint and key.** Environment, under cetra's own names. Options for the plan, as facts:
  1. `CETRA_OTLP_ENDPOINT` and `CETRA_OTLP_HEADERS` (percent-encoded, like the pi extension). Tested in every good run. Set by a launcher or by the user's shell profile. Works in both builds with no noise.
  2. A file the plugin reads (for example under `~/.config/cetra/`). Keeps the key out of the environment. Not tested here; the plugin would read a file on every start.
  3. Using `OTEL_EXPORTER_OTLP_*`: rejected by § 6.2 (noise in both builds).
  Laminar reads `LMNR_PROJECT_API_KEY` from the environment or `.env` files, so both precedents exist.
- **Content switch.** `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT`, default off, as pi. Off means no `gen_ai.input.messages`, `gen_ai.output.messages`, tool arguments or results (**run**: the `nocontent` variant was not run for OpenCode; the gate is in the prototype's `capture()` and is covered by the code path, not by a run).
- **Broken file.** Not fatal in either build (§ 6.4). Setup should parse or dry-run the file before it reports success.
- **v1 startup side effect.** Every v1 run, good or broken, logged `WARN background dependency install failed` for the config directory, because the sandbox has no network (`X/v1-final/xdg/data/opencode/log`). With a network, v1 may install plugin dependencies there; the prototype has none, and this was not tested online. v1 also creates `plugins/`, `.gitignore` and `opencode.jsonc` in the config directory (`X/broken-v1-syntax/xdg/config/opencode`).
- **Per-run opt-out.** Delete the file, or leave the endpoint unset (then the prototype sends nothing). The v2 runs set `OPENCODE_DISABLE_DEFAULT_PLUGINS=1`, which does not stop a global plugin file from loading.

## 8. Open questions

1. **Session span.** The prototype has one `invoke_agent` per turn and no session span. Does the dashboard want a session as a parent of its turns? v2 has `session.created` (§ 4.2) and v1 has the same event; neither is mapped yet.
2. **Cache semantics.** Is OpenCode's `tokens.input` exclusive of cache in every provider (Laminar's code assumes it for v2)? The fake reports no cache, so no run answers this.
3. **v1 request messages.** v1 has no per-session request hook (§ 4.2). Is a `chat` span without input acceptable on v1, or does the plan accept a correlation by message id through `experimental.chat.messages.transform`?
4. **v1 tool calls in the chat output.** The v1 output lacks the tool-call part (the v2 output has it). Fix by tracking `tool` parts by message id from `message.part.updated`, or accept the gap?
5. **v1 tool errors.** Implemented through `message.part.updated` (`state.status === "error"`), but untested: no tool run failed.
6. **Config key.** The opencode.ai plugins page says `plugins`; the Laminar README and v1 types say `plugin`. Setup needs no key (global file), so this does not block the plan, but the plan should not write a key.
7. **Subagents.** v2 `subagent` tool and v1 `task` tool create child sessions (Laminar links them). The prototype would emit a separate `invoke_agent` root for each child session. Not tested.
8. **Compaction and retries.** v2 `session.compaction.*` and the `session.hook("compaction")` are not mapped; a v2 retry creates a second `session.step.*`. Not tested.
9. **Interactive TUI.** Only `opencode run` was run (no TTY in the sandbox). Turn end on a long interactive session, and `session.idle` on v1 during TUI use, are untested.
10. **Install of v2 by users.** Which version `opencode.ai/v2/install` installs today is not stated by the page. The plan should pin the cetra support floor, not the installer's choice.
11. **Typecheck.** The prototype was run through OpenCode's own loaders, not type-checked against `@opencode/plugin` or `@opencode-ai/plugin`. The v2 hook inputs (`session.hook("context")`) are typed `any` in the prototype.
12. **Decision needed (user).** The endpoint transport (§ 7): environment with cetra names, or a cetra config file. And the plugin's name and marker line.

## 9. Network

**Nothing left loopback.** Every OpenCode run and the fake services ran in `unshare -Urn` (loopback only). In every run's `trace.strace`, every `connect()` targets `127.0.0.0/8`: the fake sink and provider on `127.0.0.1`, and UDP `connect()` to `127.0.0.53:53` (DNS stub address on loopback; a UDP connect sends no packet, and the namespace has no route out). OpenCode v1 attempted a dependency install from the npm registry during startup; that attempt failed inside the namespace (`FetchError`), and nothing left it.

Downloads and reads outside the sandbox, all from the allowed sources:

- npm registry (`registry.npmjs.org`): metadata (`npm view`) for `opencode-ai`, `@opencode-ai/plugin`, `@opencode-ai/sdk`, `@opencode/cli`, `@opencode-ai/cli`, `@lmnr-ai/opencode-plugin`, `opencode`; tarballs (`npm pack`) for `opencode-ai@1.19.6`, `@opencode-ai/plugin@1.19.6`, `@opencode-ai/sdk@1.19.6`, `@lmnr-ai/opencode-plugin@0.2.0`. Hashes in § 1.
- `opencode.ai` (reads only, through WebFetch and the Browser pane): `https://opencode.ai/docs/plugins/`, `https://opencode.ai/install` (redirect reported, not followed), `https://opencode.ai/v2/install`, `https://opencode.ai/v2/docs/plugins`, plus the Browser pane's `https://opencode.ai/docs` and `/v2/docs/plugins`.
- **Not fetched:** the raw GitHub file that `opencode.ai/install` redirects to (`raw.githubusercontent.com/anomalyco/opencode/refs/heads/v1/install`). It is not an allowed download source for this spike; its content is open (§ 8 item 10 and § 3).
- Local reads: `E/m7c/src/opencode-src` (the v2.3.0 tree, prior spike), `/home/sebastien/github/lmnr/frontend/lib/spans/types/opencode.ts`.

Nothing was committed, pushed or published.
