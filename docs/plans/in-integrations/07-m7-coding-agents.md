# IN M7 — Coding agents (part 1: Qwen Code, Pi, OpenClaw)

*Cold review (Sonnet) applied: installs moved to IN7.0, the pi prototype committed in the repo, the Qwen and OpenClaw facts recorded in D-IN-28 items 7 and 5, the helper and parameter details spelled out.*

This plan makes Qwen Code, Pi and OpenClaw first-class coding agents, as track CA did for Claude Code, Codex, Gemini CLI and Copilot (D-IN-12, D-IN-27). For each agent, cetra-otel detects its spans and sets `metadata['coding_agent']`. `cetra agents setup <agent>` and `remove` connect it, and a pinned build is captured against the mock provider. A page documents it. Pi gets a pi extension that cetra writes and ships; OpenClaw's setup installs its official diagnostics plugin. The plan also adds setup-only pages for Augment Code, the Devin SDK and Omnigent. OpenCode v2, Goose, Oh My Pi and Hermes Agent come in part 2, an amendment written when the setup-routes spike (`spikes/m7-setup-routes.md`) is in.

**Prerequisites:** M6 implemented (its records phase may still be running; this plan's last phase, IN7.8, runs after it).

**Read first:**
- `AGENTS.md`.
- `docs/plans/in-integrations/00-design.md`: § Coding agents (M7), D-IN-12, D-IN-27, D-IN-28 (below).
- `docs/mvp-decisions.md`: D-CA-1 to D-CA-17 (Track CA section).
- `docs/spec/02-ingestion.md` § Coding agents.
- `docs/spec/09-api.md` § CLI (the `cetra agents` rows).
- The spikes:
  - `spikes/m7-coding-agents.md`: § 4 Qwen Code, § 5, § 6.
  - `spikes/m7-pi-extension.md`: all of it.
  - `spikes/m7-openclaw-omnigent.md`: § 4 and § 5.
- The code:
  - `crates/otel/src/agents/{mod,gemini_cli,copilot,codex}.rs`;
  - `crates/cli/src/agents/{mod,setup,codex,hook}.rs`;
  - `crates/server/src/routes/agent_context.rs`;
  - `tools/capture/ts/{agents.ts,scenarios/gemini_cli/shell-tool/}`;
  - `tools/capture/fetch-tools.sh`.

## What it is, and what it is not

**When it is done:**
- **Detection.** cetra-otel detects three new agents and sets `metadata['coding_agent']`:
  - `qwen-code`: service name `qwen-code`;
  - `pi`: scope `cetra-pi`, the scope of cetra's own extension;
  - `openclaw`: scope `openclaw`.
  Each gets its kind, discard and tool-rename rules (§ Rules).
- **Setup.** `cetra agents setup` and `remove` support `qwen-code`, `pi` and `openclaw`:
  - Qwen Code gets the session-context hook.
  - Pi gets cetra's extension file, and setup prints the environment lines.
  - OpenClaw gets its plugin installed and its config merged.
- **Server.** `/v1/agent-context` accepts `qwen-code`.
- **Captured cases:** `qwen_code/shell-tool`, `pi/read-file` and `openclaw/read-file`.
- **Pages:**
  - `qwen-code`, `pi` and `openclaw` (captured);
  - `augment-code`, `devin-sdk` and `omnigent` (setup only).
- **Index and records:** the index's Coding agents rows, the agent lists in the marketing text and the skill, spec 02, 04 and 09, D-IN-28, the CHANGELOG and the roadmap.

**Not in this plan:**
- OpenCode v2, Goose, Oh My Pi and Hermes Agent. Part 2 covers them, written from `spikes/m7-setup-routes.md`.
- A hook for Pi or OpenClaw: neither has a hook mechanism that setup can register. Pi has no repository or branch metadata, and neither has OpenClaw.
- A `cetra pi` wrapper command or shell-profile edits (D-IN-28 item 2).
- Pi's system prompt in the captured input (D-IN-28 item 4).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-3 | A coding agent is a detection result in cetra-otel, not a `Convention` |
| D-CA-4 | `metadata['coding_agent']`, `repository`, `branch` |
| D-CA-6 | A turn is `agent`, a model call `llm`, a tool `tool` named after its tool; its original name goes to `cetra.span_name` |
| D-CA-7 | Priced by the project's table; agents' own cost figures are not read |
| D-CA-8, D-CA-9 | The hook and `/v1/agent-context` |
| D-CA-10 | `cetra agents setup/remove`, `--print`, `--no-content`, `--user`, the key from `CETRA_INGEST_KEY` |
| D-CA-14 | Content on by default in setup; `--no-content` turns it off |
| D-IN-12, D-IN-27 | The M7 scope |
| D-IN-28 (new, recorded by IN7.8) | Items below |

D-IN-28, M7 part 1's decisions:

1. **Detection keys:**
   - Qwen Code: resource `service.name` `qwen-code`.
   - Pi: instrumentation scope `cetra-pi`, which cetra's extension sets.
   - OpenClaw: instrumentation scope `openclaw`. OpenClaw's service name is configurable, so the scope is the key.
2. **Pi's endpoint and key reach pi through the environment, and setup prints the lines.** pi has no config file for them (spike § 7). `cetra agents setup pi` writes the extension file and prints, for the user's shell profile:
   - `OTEL_EXPORTER_OTLP_ENDPOINT=<url>`;
   - `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`;
   - `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (only without `--no-content`).

   It edits no shell profile. This is Copilot's print pattern (D-CA-10). Rejected: a `cetra pi` wrapper, as LangWatch built, which is a new CLI command and a process wrapper to maintain.
3. **Pi's extension:**
   - The file is `cetra.ts` in pi's global extension folder: `$PI_CODING_AGENT_DIR/extensions/` when set, else `~/.pi/agent/extensions/`.
   - Its first line is the marker `// Managed by cetra (cetra agents setup pi). Delete this file or run: cetra agents remove pi`.
   - Setup overwrites the file only when it is missing or starts with the marker; remove deletes it only when it starts with the marker.
   - The source lives in `crates/cli/src/agents/pi_extension.ts` and is embedded with `include_str!`.
   - It has no dependencies: plain `fetch` with OTLP/JSON.
4. **Pi's trace shape:**
   - one trace per `agent_start` (one prompt);
   - `invoke_agent` is the root, with `chat <model>` and `execute_tool <name>` as children;
   - `gen_ai.conversation.id` holds pi's session id on every span.

   Known gaps, accepted and listed in the page's Limits:
   - The system prompt is not captured: pi keeps it outside the messages.
   - A provider retry gives two `chat` spans.
   - A compaction summary call is a plain `chat` span.
5. **OpenClaw setup runs the plugin install.** `cetra agents setup openclaw` runs `openclaw plugins install --pin --accept-capabilities npm:@openclaw/diagnostics-otel@<v>`, where `<v>` is the version `openclaw --version` prints: the plugin is released with OpenClaw, under the same version. Then setup merges `~/.openclaw/openclaw.json`. This amends D-CA-10 ("setup writes settings only"); the user approved it (2026-11-24).
   - A plugin install from a local file is never used. OpenClaw grants the exporter capability only to an install from npm or ClawHub (spike § 4.1).
   - `--print` prints the install command instead of running it.
   - `plugins.allow` is appended to only when the user already keeps the list: an absent list means every plugin may load, and only a non-empty list restricts loading (OpenClaw 2026.11.5, `docs/tools/plugin.md` in its package; its `ensurePluginAllowlisted` helper appends only to an existing array). Setup never creates the list.
   - `remove` undoes the config merge and leaves the plugin installed. It takes back the plugin's `enabled` entry key (deleting the entry only when it is left empty) and its `plugins.allow` element when the list exists, plus the `diagnostics.otel` keys setup set. It prints `openclaw plugins uninstall diagnostics-otel` for the user to run.
6. **OpenClaw's metrics and logs are off in setup** (`metrics: false`, `logs: false`). Qwen Code's logs and metrics are accepted and dropped (D-IN-27 item 5; D-CA-2 already does this).
7. **Qwen Code's hook events are `SessionStart` and `UserPromptSubmit`.** Those are the events Qwen 0.28.0 fired in the spike. `BeforeAgent`, configured in the spike too, did not fire (`spikes/m7-coding-agents.md` evidence `runs/qwen-final/hooks.jsonl`). The payload has `session_id` and `cwd`, as Gemini's does. Facts from Qwen 0.28.0's bundled docs (`bundled/qc-helper/docs/`), read for this plan:
   - **Hooks** (`features/hooks.md`):
     - the shape is `{"hooks": {Event: [{"matcher"?: string, "hooks": [{"type": "command", "command": string, "timeout"?: number}]}]}}`, the shape `set_hooks` writes;
     - the matcher is optional;
     - a command hook's `timeout` is in **seconds** (a value of 1000 or more is read as milliseconds, for compatibility). cetra writes `5`.
   - **Telemetry** (`configuration/settings.md` § telemetry):
     - keys `enabled`, `target`, `otlpEndpoint`, `otlpProtocol` (`grpc` or `http`, where `http` is HTTP/JSON), `logPrompts`, `includeSensitiveSpanAttributes` (default `false`; it attaches prompts, tool I/O and responses to span attributes);
     - there is no `traces` key.
8. **Version rule.** The versions in § Versions are the pins: each was the newest release when this plan was written. The rule for any later pin is the newest release that is at least 7 days old at install time, the same rolling rule as pnpm's `minimumReleaseAge` (10080 minutes) and uv's `exclude-newer = "7 days"`. This settles the spikes' cut-off question (spike § 7 item 3). A pin that pnpm refuses because it is under 7 days old is installed once it is old enough; the phase waits and does not re-pick.
9. **No agents.toml for Pi.** Pi has no hook, so `cetra agents setup pi` saves no `agents.toml` (as Copilot). OpenClaw saves it, because `remove` checks the saved URL (`points_at`).
10. **`cetra agents hook pi` and `hook openclaw`** are accepted by clap, since `Agent` is one `ValueEnum`, and the server answers `400` for them (`/v1/agent-context` accepts only agents with hooks). Nothing calls them; this is accepted rather than splitting the enum.

## Rules

The kind each rule gives. "Drop" counts the span in `discarded`, as Codex's rule does.

**Qwen Code** (`agents/qwen_code.rs`; detection by service `qwen-code`):

| Span | Result |
|---|---|
| `qwen-code.interaction` | `agent` |
| `qwen-code.llm_request` | `llm` (generic GenAI reading; provider from `gen_ai.provider.name`) |
| `qwen-code.tool` | `tool`, renamed after `gen_ai.tool.name` (D-CA-6) |
| `qwen-code.hook` | drop |
| a span whose scope starts with `@opentelemetry/instrumentation-` | drop (the spike saw an undici `GET`) |

**Pi** (`agents/pi.rs`; detection by scope `cetra-pi`): `invoke_agent` is `agent`, `chat *` is `llm` and `execute_tool *` is `tool`, all from the generic GenAI reading (spike § 4). `execute_tool *` is renamed after `gen_ai.tool.name`. Nothing is dropped.

**OpenClaw** (`agents/openclaw.rs`; detection by scope `openclaw`; spike § 4.5):

| Span | Result |
|---|---|
| `openclaw.model.call` | `llm`; provider from `gen_ai.system` (generic); TTFT from `openclaw.model_call.time_to_first_byte_ms`, in milliseconds |
| `openclaw.model.usage` | drop (a per-turn total of the `model.call` spans; double counts) |
| `openclaw.run` | `agent` |
| `openclaw.harness.run` | `chain` |
| `openclaw.tool.execution` | `tool`, renamed after `gen_ai.tool.name` |
| `openclaw.diagnostic.phase`, names starting `openclaw.gateway.rpc.`, `openclaw.context.assembled` | drop |
| any other `openclaw.*` name | unchanged (generic) |

## Versions

The implementer records the publish time with `npm view <pkg> time --json` and reports the version used. These pins win (D-IN-28 item 8). If pnpm refuses one as under 7 days old, wait until it isn't: on 2026-11-24, all of them are old enough after 12:10Z.

| Package | Version | Published (UTC) | Where | Why |
|---|---|---|---|---|
| `@qwen-code/qwen-code` | 0.28.0 | 2026-11-17T12:10:30Z | `tools/capture/ts/package.json` | the Qwen capture |
| `@earendil-works/pi-coding-agent` | 1.5.2 | 2026-11-10T12:24:55Z (tarball sha256 `691bde186273b457d9d1c41d0fd47b57853b5e85ded1add1a1f6b78e66c261f4`) | `tools/capture/ts/package.json` | the Pi capture |
| `openclaw` | 2026.11.5 | 2026-11-17T09:59:33Z | new pnpm tool project `tools/capture/openclaw/` | the OpenClaw capture (its plugin install needs its own HOME, so it is a tool, not a capture dependency) |
| `@openclaw/diagnostics-otel` | 2026.11.5 | 2026-11-17T07:48Z | installed by `openclaw plugins install` in `just capture-tools` | the exporter |

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN7.0 Installs | `tools/capture/ts/{package.json,pnpm-lock.yaml,pnpm-workspace.yaml}`, new `tools/capture/openclaw/{package.json,pnpm-workspace.yaml,pnpm-lock.yaml}`, `tools/capture/fetch-tools.sh` | — |
| IN7.1 Detection | `crates/otel/src/agents/{mod,qwen_code,pi,openclaw}.rs`, `crates/otel/src/normalize.rs` (the `discarded` call site), `crates/otel/tests/proptest.rs`, `docs/spec/02-ingestion.md` | — |
| IN7.2 Pi setup and extension | `crates/cli/src/agents/{mod,setup,pi}.rs`, `crates/cli/src/agents/pi_extension.ts` (new), `crates/cli/src/agents/pi_extension.test.mjs` (new), `crates/cli/src/main.rs` (the `Agents` doc string) | — |
| IN7.3 Qwen Code setup | `crates/cli/src/agents/{mod,setup,qwen_code}.rs`, `crates/cli/src/main.rs` (doc string), `crates/server/src/routes/agent_context.rs`, `crates/server/src/api/agent_context.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` and `website/src/generated/openapi.json` (regenerated), `docs/spec/04-metadata-db.md` | IN7.2 (same files) |
| IN7.4 OpenClaw setup | `crates/cli/src/agents/{mod,setup,openclaw}.rs`, `crates/cli/src/agents/hook.rs` (visibility of `run_with_timeout` only), `crates/cli/src/main.rs` (doc string) | IN7.3 (same files) |
| IN7.5 Captures | `tools/capture/ts/agents.ts`, new `tools/capture/ts/scenarios/{qwen_code,pi,openclaw}/read-file/`, `tools/capture/src/scenario.rs` (`GROUPS`), `crates/otel/tests/support/mod.rs` (`GROUPS`), `fixtures/otlp/README.md`, new fixtures under `fixtures/otlp/{qwen_code,pi,openclaw}/` | IN7.0, IN7.1 and IN7.2 (it blesses with the rules and ships the extension file) |
| IN7.6 Setup-only pages | `website/src/content/docs/docs/integrations/{augment-code,devin-sdk,omnigent}.mdx` | — |
| IN7.7 Captured pages | `website/src/content/docs/docs/integrations/{qwen-code,pi,openclaw}.mdx` | IN7.2–IN7.5 |
| IN7.8 Index and records | `website/src/content/docs/docs/integrations/index.mdx`, `website/astro.config.mjs`, `website/src/marketing/data/{integrations.test.mjs,features.mjs}`, `website/src/marketing/data/compare/cetra.json`, `website/src/marketing/home/{CodingAgents.astro,Faq.astro}`, `website/src/content/docs/docs/tracing/custom-dashboards.mdx`, `website/src/content/docs/docs/get-started/coding-agent.mdx`, `web/src/lib/dashboards/templates.ts` (description text), `skills/cetra/SKILL.md`, `skills/cetra/references/*.md` (generated), `website/src/content/docs/docs/reference/cli.md` (generated), `docs/spec/09-api.md`, `docs/plans/in-integrations/00-design.md`, `docs/mvp-decisions.md`, `CHANGELOG.md`, `docs/roadmap.md` | IN7.6, IN7.7, and M6's IN6.9 committed |

**Parallel runs:**
- IN7.0, IN7.1, IN7.2 and IN7.6 can run at the same time: their files don't overlap.
- IN7.2 → IN7.3 → IN7.4 run in order, because they share `crates/cli/src/agents/{mod,setup}.rs`.
- IN7.5 starts after IN7.0, IN7.1 and IN7.2. It may run beside IN7.3 and IN7.4.
- IN7.8 builds on M6's committed edits to the index, sidebar, CHANGELOG and decision files (IN6.9, 9f226e1).
- Machine load: one Rust compile at a time if two Rust phases overlap. Each Rust phase compiles and tests once, right before its commit.

---

## IN7.0 — Installs

**Goal:** the three pinned builds are installed for the capture tool.

**Edit:** as the phase table says.

**Steps.**
1. **Capture dependencies.**
   - Add `@qwen-code/qwen-code` and `@earendil-works/pi-coding-agent` at the § Versions pins (exact) to `tools/capture/ts/package.json`, then run `just capture-ts-sync`.
   - If pnpm reports install scripts it blocked, add the packages it names to `allowBuilds` in `tools/capture/ts/pnpm-workspace.yaml`, as other entries there do, and report each one.
2. **OpenClaw tool project.** Create `tools/capture/openclaw/` in the shape of `tools/capture/n8n/`: `package.json` with `openclaw` 2026.11.5, `pnpm-workspace.yaml` and `pnpm-lock.yaml`.
3. **`fetch-tools.sh`:**
   - Add `pnpm_tool openclaw 2026.11.5 openclaw/package.json` after the existing `pnpm_tool` lines.
   - Then add a step that installs the plugin into a staged home, skipped when `<dir>/home/.openclaw` already holds `diagnostics-otel`:
     ```
     HOME=<dir>/home XDG_CONFIG_HOME=<dir>/home/.config XDG_DATA_HOME=<dir>/home/.local/share XDG_STATE_HOME=<dir>/home/.local/state XDG_CACHE_HOME=<dir>/home/.cache \
       <dir>/node_modules/.bin/openclaw plugins install --pin --accept-capabilities npm:@openclaw/diagnostics-otel@2026.11.5
     ```
     Here `<dir>` is `target/capture-tools/openclaw/2026.11.5`.
   - This step uses the network. It is part of `just capture-tools`, outside the capture namespace.
4. Run `just capture-tools` and `just capture-ts-sync`. Record:
   - `openclaw --version`: expected `OpenClaw 2026.11.5 (e13cf15)`;
   - the plugin's installed version;
   - the publish times.

**Done when:** both recipes succeed twice (the second run skips), and the staged home lists the plugin.

**Commit:** `Pin Qwen Code, pi and OpenClaw for captures`

## IN7.1 — Detection

**Goal:** cetra-otel detects Qwen Code, Pi and OpenClaw and applies § Rules.

**Edit:** `crates/otel/src/agents/mod.rs`, new `crates/otel/src/agents/{qwen_code,pi,openclaw}.rs`, `crates/otel/src/normalize.rs` (the `discarded` call site, about line 217), `crates/otel/tests/proptest.rs`, `docs/spec/02-ingestion.md`.

**Steps.**
1. `CodingAgent` gains `QwenCode`, `Pi`, `OpenClaw`; `wire_name` gives `"qwen-code"`, `"pi"`, `"openclaw"`.
2. `detect_agent` checks, in this order:
   - service `qwen-code` → `QwenCode`, **first**, before the existing checks: Qwen Code is a Gemini CLI fork and may carry Gemini's attribute (test below);
   - the existing four, unchanged;
   - then:
   - scope `cetra-pi` → `Pi`;
   - scope `openclaw` → `OpenClaw`.

   Exact string equality in each case.
3. Each new module follows `gemini_cli.rs`'s shape (GenAI-based agent), with the functions `mod.rs`'s match arms call. Add an arm for each new variant in:
   - `finish_inputs`: the tool name from `gen_ai.tool.name`;
   - `kind`;
   - `context`: `apply_session_context` for Qwen Code (it has the hook); nothing beyond `CODING_AGENT` for Pi and OpenClaw;
   - `usage`;
   - `ttft`: OpenClaw reads `openclaw.model_call.time_to_first_byte_ms` wherever it appears. `ttft` has no span name, and only `openclaw.model.call` carries the key.
     - Read it with the existing `count` helper, which adds `Warning::InvalidNumber` for a non-integer value; keep that warning.
     - Convert milliseconds to nanoseconds with `checked_mul(1_000_000)`. On overflow, store no TTFT and add `InvalidNumber`.
     - The key is consumed.
   - `payload`;
   - `finish`: `rename_tool` on the tool spans named in § Rules.

   Where a new agent has no special rule, its arm does what Gemini CLI's arm does.
4. `discarded` becomes a match:
   - Codex as today;
   - Qwen Code drops `qwen-code.hook` and scopes starting `@opentelemetry/instrumentation-`;
   - OpenClaw drops the names in § Rules;
   - Pi drops nothing.

   `discarded(agent, span_name, attributes)` has no scope today: add a `scope_name: &str` parameter and pass `scope_name` at its one call site in `normalize.rs`.
5. Kinds: `qwen-code.interaction` → `agent`; `openclaw.run` → `agent`; `openclaw.harness.run` → `chain`. Every other kind comes from the generic reading.
6. `proptest.rs`:
   - add `qwen-code` to `AGENT_SERVICE_NAMES`;
   - add `cetra-pi` and `openclaw` to `SCOPE_GATES` (about lines 86–92), which `scope_label` draws from.
7. Spec 02 § Coding agents:
   - the three detection rules;
   - a kind table for each agent (§ Rules);
   - the drop lists;
   - OpenClaw's TTFT key;
   - a sentence that Pi's spans come from cetra's own extension (D-IN-27 item 2).

**Tests** (unit, next to the code):
- `detect_agent_reads_each_signal` gains the three keys, plus near misses: service `qwen-codex`, scope `cetra-pi-x`, scope `openclaw.x`, each giving no agent.
- `wire_names_are_the_d_ca_4_values` covers the three new names.
- `qwen_code_spans_get_their_kinds_and_tool_name`: interaction → agent; tool → tool renamed, with `cetra.span_name` = `qwen-code.tool`; llm_request → llm.
- `qwen_code_hook_and_http_spans_are_discarded`: also checks that the same names from another service are kept.
- `pi_tool_span_is_renamed_and_kinds_are_generic`.
- `openclaw_rules`:
  - each row of the table;
  - `model.usage` dropped;
  - `openclaw.gateway.rpc.dispatch` dropped;
  - `openclaw.webhook.processed` kept;
  - TTFT 168 ms → 168 000 000 ns;
  - a non-integer TTFT is ignored without error.
- `openclaw_names_on_another_scope_are_untouched`.
- `qwen_service_wins_over_the_gemini_attribute`: service `qwen-code` plus `gen_ai.agent.name = "gemini-cli"` gives `QwenCode`.
- A Qwen span with a session context gets its repository and branch (as Gemini's test does).

**Done when:**
- `cargo nextest run -p cetra-otel`, clippy (`-D warnings`) and `cargo fmt --check` pass.
- No `expected.json` changes (stop if one does).

**Commit:** `Detect Qwen Code, Pi and OpenClaw`

## IN7.2 — Pi setup and extension

**Goal:** `cetra agents setup pi` installs cetra's pi extension and prints the environment lines. `cetra agents remove pi` removes the extension.

**Edit:**
- `crates/cli/src/agents/mod.rs`;
- `crates/cli/src/agents/setup.rs`;
- new `crates/cli/src/agents/pi.rs`;
- new `crates/cli/src/agents/pi_extension.ts`;
- `crates/cli/src/main.rs`: only the `Agents` command's doc string, which lists the agents.

**Steps.**
1. In `mod.rs`, `Agent` gains `Pi` (`#[value(name = "pi")]`, `wire_name` `"pi"`), with the doc comment updated.
2. **`pi_extension.ts`.** Start from the spike's working prototype, `docs/plans/in-integrations/spikes/m7-files/cetra-pi-prototype.ts`, which is ours. The plan's text wins where they differ.
   - The file has **no imports at all**. It uses the globals `fetch`, `crypto.getRandomValues`, `AbortSignal` and `process.env`, as the prototype does.
   - Its first line is the marker of D-IN-28 item 3, exactly.
   - pi loads it as a default-export function that receives pi's extension API. `spikes/m7-pi-extension.md` § 2 and § 6 give the event names and payload fields.
   - The file must:
   - **Turn itself off** when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset or empty: register no handler, so pi runs unchanged.
   - **Endpoint:** trim trailing `/` from the endpoint, then POST OTLP/JSON to `<endpoint>/v1/traces` with `Content-Type: application/json`.
   - **Headers:** parse `OTEL_EXPORTER_OTLP_HEADERS` as comma-separated `key=value` pairs. Split each at the first `=`, trim both sides, and percent-decode the value with `decodeURIComponent`, keeping the raw value when it throws. A `+` stays a `+`.
   - **Resource:** `service.name` from `OTEL_SERVICE_NAME`, default `pi`.
   - **Scope:** name `cetra-pi` (the prototype says `cetra-pi-extension`; change it), version the constant `EXTENSION_VERSION = "1"`.
   - **Content capture:** only when `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` is `true`, case-insensitive. The prototype also accepts `1` and `yes`; drop those.
   - **No spike knobs.** Remove the prototype's `PI_FLUSH` switch and its `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` override: only the base endpoint is read.
   - **Spans and attributes:**
     - The spike's § 6 table is the specification.
     - **Trace shape (D-IN-28 item 4):** a new trace ID at every `agent_start`, where the prototype minted one per process.
     - **Message attributes:** `gen_ai.input.messages` and `gen_ai.output.messages` are JSON strings of OpenAI chat messages, built by the prototype's `toOpenAI`:
       - `{role, content}` for system and user, with the text parts joined by `\n`;
       - for assistant: `{role: "assistant", content, reasoning_content?, tool_calls?: [{id, type: "function", function: {name, arguments: <JSON string>}}]}`;
       - for a tool result: `{role: "tool", tool_call_id, content}`;
       - other roles are skipped.
     - `finish_reasons` map pi's stop reasons as follows:
     | pi stop reason | `finish_reasons` value |
     |---|---|
     | `stop` | `stop` |
     | `length` | `length` |
     | `toolUse` | `tool_calls` |
     | `error` | `error` |
     | `aborted` | `cancelled` |
     | any other | the raw value |
   - **IDs:** trace and span IDs come from `crypto.getRandomValues`, as lowercase hex (16 and 8 bytes). Times are nanoseconds as decimal strings.
   - **Named exports** besides the default export, so a Node test can import the pure helpers: `parseHeaders(text)`, `finishReason(piReason)` and `toOpenAI(message)`. pi ignores named exports.
   - **Flush:**
     - when: at `agent_end` and at `session_shutdown`;
     - how: each flush sends the spans buffered so far in one POST, with `AbortSignal.timeout(5000)`;
     - on failure: a failed POST is dropped silently, and nothing is written to stdout or stderr.
   - **Never throw** from a handler. Wrap each handler body in `try`/`catch` that swallows the error: a thrown error must not break pi.
3. **`pi.rs`:**
   - `pi_dir(paths)`: `$PI_CODING_AGENT_DIR` when set and not empty, else `<home>/.pi/agent`. Read the variable through the same mechanism `codex_home` uses for `CODEX_HOME`.
   - `const EXTENSION: &str = include_str!("pi_extension.ts")` and `const MARKER` (the first line).
   - `setup(args, url, paths, out)`:
     - Target `<pi_dir>/extensions/cetra.ts`.
     - If the file exists and does not start with `MARKER`, fail with "`<path>` exists and was not written by cetra; move it away and run setup again" and write nothing.
     - Otherwise write `EXTENSION` through `emit`, which honours `--print`, creating the directory.
     - Then print the shell lines of D-IN-28 item 2, `export`-prefixed and shell-quoted with the existing `shell_quote` helper, after the sentence: "pi reads its endpoint and key from the environment. Add these lines to your shell profile:".
     - `--no-content` omits the content line.
   - `remove(paths, out)`:
     - If the file exists and starts with `MARKER`, delete it and say so.
     - If it exists without the marker, leave it and say it was not written by cetra.
     - If it is absent, print "no cetra pi extension found at `<path>`".
     - Then remind the user to delete the `OTEL_*` lines from their profile.
4. **`setup.rs`:**
   - `setup` and `remove` dispatch `Agent::Pi` to `pi::setup` and `pi::remove`.
   - `agents_file`: skipped for Pi, as for Copilot (D-IN-28 item 9). The condition at about line 53 becomes "not Copilot, not Pi, not `--print`".
   - The helpers `pi.rs` needs from `setup.rs` (`emit`, `shell_quote`) become `pub(super)`.
   - `hook_timeout(Agent::Pi)` is `None`, since Pi has no hook.
   - Confirm that every `match` on `Agent` in the crate compiles with the new arm. Each match arm states Pi's case explicitly; none falls through to a wildcard.
5. **`main.rs`:** the `Agents` doc string lists the agents as the code now has them.

**Tests** (in `pi.rs`, with `paths_under`, as `setup.rs`'s tests do):
- `pi_setup_writes_the_extension_with_the_marker`: the content equals `EXTENSION`, and the first line is `MARKER`.
- `pi_setup_prints_the_environment_lines`: the output contains the endpoint, the header line `Authorization=Bearer%20<key>` and the content flag.
- `pi_setup_no_content_omits_the_content_line`.
- `pi_setup_uses_pi_coding_agent_dir`.
- `pi_setup_refuses_a_foreign_file`: the file is unchanged, and an error is returned.
- `pi_setup_twice_is_idempotent`.
- `pi_print_writes_nothing`.
- `pi_remove_deletes_only_our_file`: three cases, ours, foreign and absent.
- `pi_extension_has_no_imports_outside_node`: every `import`/`require` line names `node:` modules only.
- `pi_extension_scope_is_the_detection_key`: the file contains `"cetra-pi"`. This is a literal assertion; `cetra-cli` cannot see cetra-otel's constant.
- `crates/cli/src/agents/pi_extension.test.mjs` (`node --test`), which imports the `.ts` file through Node's type stripping. Node 26 runs `.ts` directly; if it can't, stop and ask. It covers:
  - `parseHeaders`: `Authorization=Bearer%20k` gives `Bearer k`; `a=1, b=x%2Cy` gives two headers; a lone `%` stays raw; `+` stays `+`; an entry without `=` is skipped;
  - `finishReason`: each row of the table;
  - `toOpenAI`: an assistant message with a tool call, a tool result, and a custom role giving `null`.
- `pi_extension_node_tests_pass`: a Rust `#[test]` that runs `node --test` on that file and asserts success, skipping with a message when `node` is not on `PATH`. IN7.5's capture covers the event handling end to end.

**Done when:**
- `cargo nextest run -p cetra-cli`, clippy and fmt pass.
- `cli_reference_is_up_to_date` is regenerated with `CETRA_DOCS_WRITE=1` if the doc string changed; IN7.8 commits the generated page, so report it.

**Commit:** `Add cetra agents setup pi with a cetra pi extension`

## IN7.3 — Qwen Code setup

**Goal:** `cetra agents setup qwen-code` and `remove`, with the hook. `/v1/agent-context` accepts `qwen-code`.

**Edit:** `crates/cli/src/agents/{mod,setup}.rs`, new `crates/cli/src/agents/qwen_code.rs`, `crates/server/src/routes/agent_context.rs`, `crates/server/src/api/agent_context.rs`, the regenerated OpenAPI files (`just openapi`), `docs/spec/04-metadata-db.md` (the `agent` column comment).

**Steps.**
1. `Agent` gains `QwenCode` (`#[value(name = "qwen-code")]`, wire `"qwen-code"`).
2. **`qwen_code.rs`** mirrors `gemini_cli_setup` and `gemini_cli_remove` (Qwen Code is a Gemini CLI fork):
   - **Directory:** `qwen_dir(paths)` = `<home>/.qwen`.
   - **`settings.json` `telemetry` keys:** `enabled: true`, `target: "local"`, `otlpEndpoint: <url>`, `otlpProtocol: "http"`, `logPrompts: !no_content`, `includeSensitiveSpanAttributes: !no_content` (keys confirmed in D-IN-28 item 7; there is no `traces` key). `QWEN_TELEMETRY` is the const list of these six keys; remove uses it.
     - Before writing `includeSensitiveSpanAttributes`, confirm in Qwen's bundled settings docs (`node_modules/@qwen-code/qwen-code/bundled/qc-helper/docs/configuration/settings.md`, after IN7.5's install, or the spike's copy under the scratchpad's `pkgs/qwen/`) that it gates span content. If it doesn't, stop and ask.
     - There is no `traces` key: Qwen's telemetry schema has none. Confirm this in the same docs; if it has one, set it `true` and say so.
   - **Hooks:** `set_hooks` with `QWEN_EVENTS = ["SessionStart", "UserPromptSubmit"]` (D-IN-28 item 7) and `hook_command(paths, Agent::QwenCode)`.
     - The `timeout` is in seconds (D-IN-28 item 7): `hook_timeout(Agent::QwenCode)` is `Some(5)`.
   - **`.env`:** `<qwen_dir>/.env` gets the header line through `env_with_header`. Rename `gemini_header_line` to `otlp_header_line`, since it is now shared, and update its callers.
   - **No copy of Gemini's functions.** Write one function pair used by both Gemini and Qwen: `settings_env_setup` and `settings_env_remove`.
     - They take a `SettingsAgent` struct `{dir, telemetry: Vec<(&str, Value)>, removable: &[&str], events: &[&str], agent: Agent, note: &str}`.
     - Move Gemini's body into them, and make `gemini_cli_setup` and `gemini_cli_remove` call them.
     - The pair lives in `setup.rs`. `qwen_code.rs` holds only Qwen's constants and the call.
     - The Gemini tests must pass unchanged.
   - **Output:** print "Qwen Code also sends logs and metrics; cetra accepts and drops them." as Gemini does.
   - **Remove:** remove mirrors Gemini's: the telemetry keys pointing at the saved URL, our hooks, and our header line.
3. **`setup.rs`:** dispatch both. `is_our_hook` already matches `agents hook <wire_name>`, so confirm it covers `qwen-code`.
4. **`main.rs`:** the `Agents` doc string lists Qwen Code.
5. **`hook.rs`:** check that it works for `Agent::QwenCode` unchanged (the payload has `session_id` and `cwd`). Add the arm only if a match needs it.
6. **Server:**
   - `AGENTS` in `routes/agent_context.rs` gains `"qwen-code"`, so the array grows to 5, and its error message lists it.
   - Update the doc comment in `api/agent_context.rs`.
   - Run `just openapi` and keep the regenerated files.
   - Update the spec 04 column comment.

**Tests:**
- `qwen_setup_writes_settings_hooks_and_env`.
- `qwen_setup_twice_is_idempotent`.
- `qwen_no_content_turns_content_off`.
- `qwen_remove_restores`.
- `qwen_hook_timeout_unit`.
- `agent_context_accepts_qwen_code`, a new server test next to the existing agent-context route tests (they use only `codex`): `qwen-code` gives `204`, and `pi` gives `400` with the five-agent message.
- The Gemini tests still pass after the rename.

**Done when:** `cargo nextest run -p cetra-cli -p cetra-server`, clippy and fmt pass; `just openapi` leaves no diff after the commit.

**Commit:** `Add cetra agents setup qwen-code`

## IN7.4 — OpenClaw setup

**Goal:** `cetra agents setup openclaw` installs OpenClaw's diagnostics plugin and merges its config. `remove` undoes the merge.

**Edit:** `crates/cli/src/agents/{mod,setup}.rs`, new `crates/cli/src/agents/openclaw.rs`.

**Steps.**
1. `Agent` gains `OpenClaw` (`#[value(name = "openclaw")]`, wire `"openclaw"`). `hook_timeout` is `None`.
2. **`openclaw.rs`:**
   - **Directory:** `openclaw_dir(paths)` = `<home>/.openclaw`.
   - **Version:** `parse_openclaw_version(output: &str) -> Option<String>` takes the first whitespace-separated token matching `^\d{4}\.\d{1,2}\.\d{1,2}$`.
     - OpenClaw 2026.11.5's `openclaw --version` prints `OpenClaw 2026.11.5 (e13cf15)`, which gives `2026.11.5`.
     - The call uses the existing `hook::run_with_timeout` (`hook.rs`, about line 187) with program `openclaw`, args `["--version"]`, no extra environment and 10 s. Make it `pub(super)` if needed.
     - When it returns `None`, fail with "openclaw is not installed or not on PATH". When parsing fails, fail with "could not read the OpenClaw version from `openclaw --version`".
     - Before writing the parser, run `target/capture-tools/openclaw/2026.11.5/node_modules/.bin/openclaw --version` (after IN7.5's fetch step, or the spike's `E/pkgs/openclaw`). Record its exact output in a test. If the output has no such token, stop and ask.
     - If `openclaw` is not on `PATH`, fail with "openclaw is not installed or not on PATH".
   - **Plugin install:**
     - Run `openclaw plugins install --pin --accept-capabilities npm:@openclaw/diagnostics-otel@<version>`, inheriting stdout and stderr, with no timeout.
     - On a non-zero exit, fail with "the plugin install failed; nothing was written", before writing any config.
     - Under `--print`, print the command and run nothing.
   - **Config:** merge `<openclaw_dir>/openclaw.json` with the JSON helpers (`read_json_object`, `object_entry`, `write_settings` through `emit`):
     - `plugins.allow`: appended to only when it already exists as an array (an absent list means every plugin may load, and only a non-empty list restricts loading); add `"diagnostics-otel"` if absent and keep the others.
     - `plugins.entries."diagnostics-otel".enabled = true`.
     - `diagnostics.enabled = true`.
     - `diagnostics.otel` = `{ enabled: true, endpoint: <url>, protocol: "http/protobuf", serviceName: "openclaw", traces: true, metrics: false, logs: false, captureContent: !no_content, headers: { Authorization: "Bearer <key>" } }`.
       - Set each key and leave other keys of `diagnostics.otel` in place.
       - The header value is the plain form, not percent-encoded: it is JSON, not an environment variable.
   - **Printed afterwards:** "Restart the OpenClaw gateway if one is running (openclaw gateway restart). One-shot runs (openclaw agent --local) pick it up now."
   - **Remove:** when `diagnostics.otel.endpoint` points at the saved URL (`points_at`):
     - Remove the keys setup set from `diagnostics.otel`.
     - Remove `plugins.entries."diagnostics-otel".enabled`, deleting the entry only when it is left empty, and `"diagnostics-otel"` from `plugins.allow` when that list exists.
     - Print `openclaw plugins uninstall diagnostics-otel` as a suggestion.
     - Leave `diagnostics.enabled` as it is: it may predate setup.
3. **Commands are parameters, for hermetic tests.** `openclaw::setup(args, url, paths, out, version: &dyn Fn() -> anyhow::Result<String>, install: &dyn Fn(&[&str]) -> anyhow::Result<()>)`.
   - `version` runs and parses `openclaw --version` as above.
   - `install` gets the arguments after the program name: `["plugins", "install", "--pin", "--accept-capabilities", "npm:@openclaw/diagnostics-otel@<v>"]`. The real one runs `std::process::Command::new("openclaw")` with inherited stdout and stderr and no timeout, and fails on a non-zero exit.
   - `setup.rs`'s dispatch passes the real closures; the tests pass recording closures. No trait (AGENTS.md).
   - `main.rs`'s `Agents` doc string lists OpenClaw. `hook_timeout(Agent::OpenClaw)` is `None`, as an explicit arm.

**Tests:**
- `openclaw_setup_installs_the_matching_plugin_then_merges`: the fake runner records the commands; version `2026.11.5` gives the install argument `npm:@openclaw/diagnostics-otel@2026.11.5`.
- `openclaw_setup_keeps_other_plugins_and_keys`.
- `openclaw_setup_does_not_create_an_allow_list`; `openclaw_setup_appends_to_an_existing_allow_list`.
- `openclaw_remove_keeps_other_plugin_entry_keys`.
- `openclaw_setup_twice_is_idempotent`.
- `openclaw_install_failure_writes_nothing`.
- `openclaw_print_runs_nothing_and_writes_nothing`.
- `openclaw_no_content_sets_capture_false`.
- `openclaw_version_parses_the_recorded_output`.
- `openclaw_missing_binary_message`.
- `openclaw_remove_undoes_the_merge`.
- `openclaw_remove_leaves_another_endpoint`.

**Done when:** `cargo nextest run -p cetra-cli`, clippy and fmt pass.

**Commit:** `Add cetra agents setup openclaw`

## IN7.5 — Captures

**Goal:** three captured cases from pinned builds against the mock provider.

**Edit:** as the phase table says.

**Steps.**
1. **Groups:** add `qwen_code`, `pi` and `openclaw` to `GROUPS` in `tools/capture/src/scenario.rs` and `crates/otel/tests/support/mod.rs`, and to the group list in `fixtures/otlp/README.md`. Extend `case_path_accepts_the_coding_agent_groups`.
2. The installs are IN7.0's.
3. **`agents.ts`:**
   - `AgentRun` gains `homeDirs?: Record<string, string>`: directories copied recursively into HOME before the run, with the name relative to HOME and the value an absolute source path.
   - It also gains `repoFiles?: Record<string, string>`: files written into the repo that `runAgent` creates, before the run.
   - Binary lookup accepts an absolute `bin`. OpenClaw's is `path.join(process.env.CETRA_CAPTURE_TOOLS, "openclaw/2026.11.5/node_modules/.bin/openclaw")`.
4. **OpenClaw's library version.** `writeMeta(aliases)` reads `tools/capture/ts/node_modules/<alias>/package.json`, and openclaw lives in the tool project. Pass the alias `{ openclaw: "../../../../target/capture-tools/openclaw/2026.11.5/node_modules/openclaw" }`, which `path.join` resolves from `tools/capture/ts/node_modules`. List `openclaw` in the scenario's `libraries`. `harness.ts` does not change.
5. **Scenarios.** Each is a `read-file` case: the model asks for one read of `hello.txt` (content `hello from cetra\n`, through `repoFiles`), then answers.
   - Use Gemini's `shell-tool` scenario as the shape.
   - The turns use `"api": "openai.chat"` (the mock's Chat Completions API, `tools/capture/src/scenario.rs` about line 247).
   - Arguments per tool, from the spikes:
   - **`qwen_code/shell-tool`** (amended 2026-11-24: Qwen's `read_file` requires an absolute path, which a static turn cannot give, so the case calls Qwen's shell tool with `cat hello.txt`, as Gemini's `shell-tool` case does):
     - Run `qwen --auth-type openai -m gpt-4o-mini --approval-mode yolo "<prompt>"` with `OPENAI_BASE_URL=<mock>/v1`.
     - `.qwen/settings.json` holds the telemetry block of IN7.3, with `otlpEndpoint` = the receiver and `logPrompts: true`. Use no hooks.
     - `.qwen/.env` holds the header line.
     - The tool name and its arguments are those Qwen's read tool takes: read them from the spike's `runs/qwen-final/norm.jsonl` and use them exactly.
   - **`pi/read-file`:**
     - `homeFiles` has `.pi/agent/models.json`, with provider `openai` (`baseUrl` = mock `/v1`, `api: "openai-completions"`, `apiKey` = the mock key, model `gpt-4o-mini`), as in the spike runner. It also has `.pi/agent/extensions/cetra.ts`, the content of `crates/cli/src/agents/pi_extension.ts`, read at run time.
     - The environment is the three `OTEL_*` lines aimed at the receiver.
     - Run `pi -p --model openai/gpt-4o-mini "<prompt>"`.
   - **`openclaw/read-file`:**
     - `homeDirs` copies `.openclaw` from the staged home.
     - `homeFiles` overwrites `.openclaw/openclaw.json` with the spike's `oc-local.sh` config: provider named `openai` (`api: "openai-completions"`), `diagnostics.otel` aimed at the receiver, `captureContent: true`, `metrics: false`, `logs: false`, and `agents.defaults.workspace` = the repo.
     - Run `openclaw agent --local --json -m "<prompt>"`.
   - Set `volatile_attributes` and `timing_attributes` from what `--check` shows varying between two runs. Each entry needs a reason in `notes`.
6. **Capture, check and bless:**
   - Capture with `just capture <group>/read-file`, then `just _capture-namespace <case> --check` for determinism.
   - Bless with `just fixtures-bless`, `fixtures-fmt` and `fixtures-check`.
   - **Expected:**
     - **Qwen Code:** agent, llm ×2, tool (renamed `read_file` or Qwen's tool name), with `coding_agent` `qwen-code`.
     - **Pi:** agent, llm ×2, tool `read`, with `coding_agent` `pi`.
     - **OpenClaw:** chain (`harness.run`), agent (`run`), llm ×2, tool `read`, with `context.assembled` discarded and `coding_agent` `openclaw`.
     - All three: provider `openai`, tokens as the turns say, cost `computed`.
   - Stop if any differs.

**Done when:**
- the three cases pass `--check`;
- `cargo nextest run -p cetra-otel` passes, which runs the golden fixtures;
- the capture tool's tests pass (`cargo nextest run -p cetra-capture`);
- `tools/capture/ts`'s lint and type check pass (the recipe `check-capture` uses for TypeScript).

**Commit:** `Capture Qwen Code, Pi and OpenClaw`

## IN7.6 — Setup-only pages

**Goal:** three setup-only pages, each in the shape of `website/src/content/docs/docs/integrations/cleanlab.mdx`. Every code fence gets `{/* untested: <reason> */}`.

| Slug | Title | Content, from the spikes |
|---|---|---|
| `augment-code` | Augment Code | Auggie's `otel` extension:<br>• turn on with `AUGGIE_ENABLE_TELEMETRY=1` and `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_HEADERS` (percent-encoded `Bearer%20<key>`)<br>• content off unless `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`; `OTEL_LOG_USER_PROMPTS` is separate<br>Limits:<br>• not run (it needs an Augment login)<br>• its resource carries the user's email (`AUGGIE_USER_EMAIL` / `AUGMENT_USER_EMAIL`) when set (D-IN-27 item 4)<br>• it also sends logs and metrics, which cetra drops<br>Source: `spikes/m7-coding-agents.md` § Augment Code |
| `devin-sdk` | Devin SDK | `@cognition-ai/sdk/tracing`:<br>• the app owns the tracer provider<br>• point an OTLP/HTTP exporter at cetra with the bearer header<br>• prompts are recorded by default<br>• cloud sessions have no generation spans<br>Not run. Read the SDK README in the spike's `pkgs/devin/node_modules/@cognition-ai/sdk/README.md` for the exact API; don't write a call you can't find there<br>Source: § Devin SDK |
| `omnigent` | Omnigent | the env block:<br>• `OMNIGENT_TELEMETRY_ENABLED=true`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS` (percent-encoded)<br>• `OMNIGENT_OTEL_CAPTURE_CONTENT=true` for content<br>• `OMNIGENT_OTEL_HTTP_CLIENT_INSTRUMENTATION=false` and `OMNIGENT_OTEL_FASTAPI_INSTRUMENTATION=false`<br>Limits:<br>• no per-call model spans, only the turn's total on the `agent:<name>` span, whose model field is the harness name<br>• FastAPI's own telemetry still emits plumbing spans<br>• it sends logs and metrics too<br>Source: `spikes/m7-openclaw-omnigent.md` § 5 |

**Done when:** `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs` pass.

**Commit:** `Add Augment Code, Devin SDK and Omnigent pages`

## IN7.7 — Captured pages

**Goal:** `qwen-code`, `pi` and `openclaw` pages in the shape of `gemini-cli.mdx`:
- `<Backing fixtures={["<group>/read-file"]} />`;
- What you get, from `expected.json`;
- setup with `cetra agents setup <agent>`, and the manual setup as written by IN7.2–IN7.4;
- Limits.

**Limits:**
- **Qwen Code:**
  - it also sends logs and metrics;
  - the hook events.
- **Pi:**
  - the environment lines must be in the shell profile;
  - D-IN-28 item 4's gaps;
  - the interactive TUI was not captured, only `pi -p`;
  - a broken extension file stops pi, and setup only writes the file tested with pi 1.5.2.
- **OpenClaw:**
  - setup installs the plugin from npm, and a plugin installed from a local file exports nothing;
  - the gateway route adds startup spans, which cetra drops;
  - the page documents both the `--local` and the gateway route.

**Done when:** the IN7.6 checks pass, plus `node scripts/check-backing.mjs` and `node scripts/check-integrations.mjs`. A page not yet in the index fails `check-integrations` only if that script requires every page to be listed; then say so and leave the fix to IN7.8.

**Commit:** `Add Qwen Code, Pi and OpenClaw pages`

## IN7.8 — Index and records

**Goal:** list the new agents everywhere the four CA agents are listed, and record D-IN-28.

**Steps.**
1. **Index:**
   - `index.mdx` § Coding agents: rows and cards for Qwen Code, Pi and OpenClaw (captured), and for Augment Code, Devin SDK and Omnigent (setup only).
   - The sidebar entries go after Gemini CLI.
   - `integrations.test.mjs`'s expected entries.
2. **Agent lists.** Update each, naming every first-class agent:
   - `features.mjs`;
   - `CodingAgents.astro`;
   - `Faq.astro`;
   - `custom-dashboards.mdx`;
   - `get-started/coding-agent.mdx`;
   - `skills/cetra/SKILL.md`;
   - `compare/cetra.json` (the sentence listing the agents set up by `cetra agents setup`; leave the other vendors' files alone);
   - `templates.ts`'s description: change the parenthesised list, or drop it if it no longer fits one line, and update `templates.test.ts` only if it asserts the text.
3. **Spec 09:** add the setup and remove rows for the three agents.
4. **Design doc** (`00-design.md`):
   - D-IN-28;
   - the § Coding agents rows as built;
   - the M7 milestone line: "part 1 done; part 2: OpenCode, Goose, Oh My Pi, Hermes".
5. **`mvp-decisions.md`:** D-IN-28 in the Track IN section. D-IN-27 is there already, from M6's IN6.9; check that it is.
6. **CHANGELOG:**
   - `Added`: the three agents, `cetra agents setup qwen-code|pi|openclaw`, and the six pages;
   - `Changed`: `/v1/agent-context` accepts `qwen-code`.
7. **Roadmap:** "M7 part 1 implemented" and D-IN-28.
8. **Regenerate:** `just docs-gen`, for the CLI reference and the skill references.

**Done when:** `just check-docs` passes; `cd web && pnpm vitest run src/lib/dashboards` passes if `templates.ts` changed.

**Commit:** `List the new coding agents and record D-IN-28`

## After the last phase

`just check`, a Sonnet code review, fixes by a GLM agent, then a push to PR #242.
