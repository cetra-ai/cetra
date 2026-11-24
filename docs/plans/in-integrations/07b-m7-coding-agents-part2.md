# IN M7 — Coding agents (part 2: OpenCode, Goose, Oh My Pi, Hermes Agent)

*Cold review (Sonnet, two rounds) applied: auxiliary model calls and probes handled in the mock, Hermes's provider and exit, the OMP remove without a backup; capture metadata for non-npm tools (`writeMeta` versions), the configs, turns and endpoints inlined, the plugin's helper contracts as code, no module-load I/O, one shared tool rename in cetra-otel, the CLI reference regenerated per phase, untested-fence markers.*

This plan makes OpenCode, Goose and Oh My Pi first-class coding agents, as part 1 (`07-m7-coding-agents.md`) did for Qwen Code, Pi and OpenClaw. It also adds detection and a captured page for Hermes Agent. For each agent, cetra-otel detects its spans and sets `metadata['coding_agent']`. For the first three, `cetra agents setup <agent>` and `remove` connect it. A pinned build of each of the four is captured against the mock provider, and each gets a page. OpenCode gets an OpenCode plugin that cetra writes and ships, modelled on Laminar's: it serves both the npm 1.x build and the v2 build, and reads its endpoint and key from a file that setup writes (D-IN-29).

**Prerequisites:**
- Part 1 implemented through IN7.8. This plan builds on IN7.5's capture runner extensions (`homeDirs`, `repoFiles`, absolute `bin`) and on the files IN7.2 to IN7.4 and IN7.8 edit.
- The plan's branch is `claude/in-integrations`, worktree `.claude/worktrees/integrations`.

**Read first:**
- `AGENTS.md`.
- `docs/plans/in-integrations/00-design.md`: § Coding agents (M7), D-IN-12, D-IN-27, D-IN-28, **D-IN-29**.
- `docs/plans/in-integrations/07-m7-coding-agents.md`: § Decisions (D-IN-28), § Rules, IN7.2 (the pi extension: this plan's OpenCode plugin follows it), IN7.5 (captures).
- `docs/mvp-decisions.md`: D-CA-1 to D-CA-17 (Track CA section).
- `docs/spec/02-ingestion.md` § Coding agents.
- The spikes:
  - `spikes/m7-opencode-plugin.md`: all of it.
  - `spikes/m7-setup-routes.md`: § 2, § 5, § 6, § 10.
  - `spikes/m7-coding-agents.md`: § Hermes Agent, and the R6 row of its rule table.
- The code:
  - `crates/otel/src/agents/{mod,pi,openclaw,qwen_code}.rs`;
  - `crates/cli/src/agents/{mod,setup,pi,openclaw}.rs`, `crates/cli/src/agents/pi_extension.ts` and `pi_extension.test.mjs`;
  - `tools/capture/fetch-tools.sh`, `tools/capture/ts/agents.ts`, `tools/capture/ts/scenarios/{pi,openclaw}/read-file/`;
  - the prototype `docs/plans/in-integrations/spikes/m7-files/cetra-opencode-prototype.ts` (ours, from the spike).

## What it is, and what it is not

**When it is done:**
- **Detection.** cetra-otel detects four more agents and sets `metadata['coding_agent']`:
  - `opencode`: scope `cetra-opencode`, the scope of cetra's own plugin;
  - `goose`: scope `goose`;
  - `oh-my-pi`: scope `@oh-my-pi/pi-agent-core`;
  - `hermes-agent`: scope `hermes-otel-plugin`.
  Each gets its kind and tool-rename rules (§ Rules).
- **Setup.** `cetra agents setup` and `remove` support `opencode`, `goose` and `oh-my-pi`:
  - OpenCode gets cetra's plugin file and its config file.
  - Goose gets printed environment lines; nothing is written.
  - Oh My Pi gets its `.env` lines.
- **Captured cases:** `opencode/read-file` (v2), `opencode/read-file-v1`, `goose/read-file`, `oh_my_pi/read-file`, `hermes_agent/read-file`.
- **Pages:** `opencode`, `goose`, `oh-my-pi`, `hermes-agent`, all captured.
- **Index and records:** the index's Coding agents rows, the agent lists, spec 02 and 09, D-IN-29, the CHANGELOG and the roadmap.

**Not in this plan:**
- `cetra agents setup hermes-agent`: setup cannot install a Python plugin into Hermes's environment (D-IN-29 item 7).
- A hook for any of the four, and so no repository or branch: none has a hook that setup can register.
- OpenCode's own exporter, and the community plugin `@devtheops/opencode-plugin-otel` (D-IN-29 item 1).
- Copilot's printed header and content lines (a possible follow-up from `spikes/m7-setup-routes.md` § 7 item 8; out of M7).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-3, D-CA-4 | Detection in cetra-otel; `metadata['coding_agent']` |
| D-CA-6 | A turn is `agent`, a model call `llm`, a tool `tool` named after its tool; the original name goes to `cetra.span_name` |
| D-CA-7 | Priced by the project's table; agents' own cost figures are not read |
| D-CA-10 | `cetra agents setup/remove`, `--print`, `--no-content`, the key from `CETRA_INGEST_KEY` |
| D-CA-14 | Content on by default in setup; `--no-content` turns it off |
| D-IN-28 | Part 1's decisions; items 3 (marker file), 8 (version rule) and 9 (`agents.toml`) apply here too |
| D-IN-29 | This plan's scope and rules (in `00-design.md`; recorded in `mvp-decisions.md` by IN7.15) |

The user's answers (2026-11-24):
- OpenCode: "the Laminar model ... delivered by `cetra agents setup opencode`".
- The plugin's endpoint and key: a config file written by setup, not environment variables.

Details this plan fixes, within D-IN-29:

1. **CLI names and wire names:** `opencode`, `goose`, `oh-my-pi`. Hermes's wire name is `hermes-agent` (detection only).
2. **OpenCode's folder:** `$XDG_CONFIG_HOME/opencode` when `XDG_CONFIG_HOME` is set and not empty, else `<home>/.config/opencode`. Setup and the plugin compute it with the same rule.
3. **OpenCode's files:**
   - plugin: `<folder>/plugins/cetra-opencode.ts`, first line `// Managed by cetra (cetra agents setup opencode). Delete this file or run: cetra agents remove opencode`;
   - config: `<folder>/cetra-opencode.json`, mode 0600, in this exact shape (keys in this order, two-space indent, final newline):
     ```json
     {
       "managed_by": "cetra agents setup opencode",
       "endpoint": "<url>",
       "headers": {
         "Authorization": "Bearer <key>"
       },
       "capture_content": true
     }
     ```
   Setup overwrites each file only when it is missing or is ours (the plugin starts with its marker; the config's `managed_by` equals the string above). Remove deletes each only when it is ours. The header value is plain, not percent-encoded: it is JSON, not an environment variable.
4. **No `agents.toml` for OpenCode or Goose**, as for Pi (D-IN-28 item 9): neither has a hook, and remove recognises OpenCode's files by their markers. **Oh My Pi saves it**, because its `.env` lines have no marker and remove must check they point at the saved server.
5. **Goose's printed lines** (Copilot's pattern): `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (without `--no-content`), `OTEL_LOGS_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none`. Goose decodes `%20` (`spikes/m7-coding-agents.md`, `hdr-goose`) and the plain form (`spikes/m7-setup-routes.md` § 5.2); the percent form matches Pi's lines and the OTel environment spec.
6. **Oh My Pi's `.env` lines** (`<home>/.omp/agent/.env`), in this order:
   - `OTEL_EXPORTER_OTLP_ENDPOINT=<url>`;
   - `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <key>` (plain space: the form the spike verified for Oh My Pi's reader, `omp-dotenv-content`; it is a dotenv value, not shell);
   - `OTEL_SERVICE_NAME=omp`;
   - `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` (without `--no-content`);
   - `OTEL_LOGS_EXPORTER=none`;
   - `OTEL_METRICS_EXPORTER=none`.
7. **Pins:** § Versions. Each is the newest release at least 7 days old when the spikes ran; they win over later releases (D-IN-28 item 8).

## Rules

The kind each rule gives. Nothing in this plan is dropped: every wrapper becomes `chain`, so no span loses its parent.

**OpenCode** (scope `cetra-opencode`; no module of its own, IN7.10 step 3): as Pi. `invoke_agent` is `agent`, `chat *` is `llm` and `execute_tool *` is `tool`, from the generic GenAI reading. `execute_tool *` is renamed after `gen_ai.tool.name`.

**Goose** (`agents/goose.rs`; scope `goose`; `spikes/m7-setup-routes.md` § 5.2):

| Span | Result |
|---|---|
| `reply` | `agent` (its turn-total tokens are not read: usage is read on `llm` and `embedding` spans only, `usage.rs`) |
| `chat` | `chain`: its child `stream_response_from_provider` repeats its tokens |
| `stream_response_from_provider` | `llm` (generic reading: it has the tokens, the model and the messages) |
| `execute_tool` | `tool`, renamed after `gen_ai.tool.name` |
| any other name | unchanged (generic) |

**Oh My Pi** (scope `@oh-my-pi/pi-agent-core`; no module of its own, IN7.10 step 3): `invoke_agent` is `agent`, `chat *` is `llm`, `execute_tool *` is `tool`, from the generic reading. `execute_tool *` is renamed after `gen_ai.tool.name`. The `omp.gen_ai.agent.usage.*` attributes are not read.

**Hermes Agent** (`agents/hermes.rs`; scope `hermes-otel-plugin`; spike R6):

| Span | Result |
|---|---|
| `agent` | `agent` |
| names starting `api.` | `llm` (generic reading) |
| names starting `llm.` | `chain`: a wrapper without tokens, the parent of the `api.` calls and the tool |
| names starting `tool.` | `tool`, renamed after `gen_ai.tool.name` |
| any other name | unchanged (generic) |

## Versions

The implementer re-checks each with `npm view <pkg> time --json`, the GitHub release page or `git log -1 --format=%cI <tag>`, and reports the version used. These pins win (D-IN-28 item 8).

| What | Version | Published (UTC) | Source and checksum | Where |
|---|---|---|---|---|
| OpenCode 1.x (`opencode-ai`, with its platform package `opencode-linux-x64`) | 1.19.6 | 2026-11-17T07:22:20Z | npm; `opencode-linux-x64` binary sha256 `aab0a58cd7fc5a1f17495b76c08e79989d331653d8b63156aca2f6e9e36d1173` | new pnpm tool project `tools/capture/opencode/` |
| OpenCode v2 | 2.3.0 | 2026-11-13T08:55:07Z | `https://github.com/anomalyco/opencode/releases/download/v2.3.0/opencode-linux-x64.tar.gz`, sha256 `fc0f81522aceeabac47693d88389efa2bc517ce74c2c38fd6967819fcc6e4d3b`; extracted binary sha256 `2e0e4656521ca56e5ba16a00d48429958b8105b3fa6466710a4350e68759381f` | `fetch` in `fetch-tools.sh` |
| Goose | 1.59.2 | 2026-11-16T05:26:30Z | `https://github.com/aaif-goose/goose/releases/download/v1.59.2/goose-x86_64-unknown-linux-gnu.tar.gz`, sha256 `c63cf82953e968d84b7542eb38a621dfdb5050b967728ed363517be5cdc666ea` | `fetch` in `fetch-tools.sh` |
| Oh My Pi (`@oh-my-pi/pi-coding-agent`) | 18.22.0 | 2026-11-16T22:10:38Z | npm | new pnpm tool project `tools/capture/oh-my-pi/` |
| Bun (`bun`, Oh My Pi's runtime) | 1.4.4 | 2026-10-22T10:19:13Z | npm | the same project |
| Hermes Agent | git tag `v0.23.2` | commit 2026-11-16T19:28:45-08:00 | `https://github.com/NousResearch/hermes-agent`, pinned by commit in `uv.lock` | new uv tool project `tools/capture/hermes/` |
| `hermes-otel` | git tag `hermes-otel-v1.33.0` | commit 2026-11-11T14:50:22-05:00 | `https://github.com/briancaffey/hermes-otel`, pinned by commit in `uv.lock` | the same project |

Neither Hermes package is on PyPI at these versions (PyPI's `hermes-agent` stops at 0.19.0, and `hermes-otel` is absent; checked 2026-11-24).

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN7.9 Installs | `tools/capture/fetch-tools.sh`; new `tools/capture/opencode/{package.json,pnpm-workspace.yaml,pnpm-lock.yaml}`, `tools/capture/oh-my-pi/{package.json,pnpm-workspace.yaml,pnpm-lock.yaml}`, `tools/capture/hermes/{pyproject.toml,uv.lock}` | part 1's IN7.0 (same script) |
| IN7.10 Detection | `crates/otel/src/agents/mod.rs`, new `crates/otel/src/agents/{goose,hermes}.rs`, new `crates/otel/src/agents/generic_agent_tests.rs`, deleted `crates/otel/src/agents/pi.rs`, `crates/otel/tests/proptest.rs`, `docs/spec/02-ingestion.md` | part 1's IN7.1 |
| IN7.11 OpenCode setup and plugin | `crates/cli/src/agents/{mod,setup}.rs`, new `crates/cli/src/agents/{opencode.rs,opencode_plugin.ts,opencode_plugin.test.mjs}`, `crates/cli/src/main.rs` (the `Agents` doc string), `website/src/content/docs/docs/reference/cli.md` (regenerated) | part 1's IN7.4 |
| IN7.12 Goose and Oh My Pi setup | `crates/cli/src/agents/{mod,setup}.rs`, new `crates/cli/src/agents/{goose,oh_my_pi}.rs`, `crates/cli/src/main.rs` (doc string), `website/src/content/docs/docs/reference/cli.md` (regenerated) | IN7.11 (same files) |
| IN7.13 Captures | `tools/capture/src/mock.rs` (auxiliary turns, probes), `tools/capture/ts/harness.ts` (`writeMeta`), new `tools/capture/ts/scenarios/{opencode/read-file,opencode/read-file-v1,goose/read-file,oh_my_pi/read-file,hermes_agent/read-file}/`, `tools/capture/src/scenario.rs` (`GROUPS`), `crates/otel/tests/support/mod.rs` (`GROUPS`), `fixtures/otlp/README.md`, new fixtures under `fixtures/otlp/{opencode,goose,oh_my_pi,hermes_agent}/` | IN7.9, IN7.10, IN7.11; part 1's IN7.5 |
| IN7.14 Pages | new `website/src/content/docs/docs/integrations/{opencode,goose,oh-my-pi,hermes-agent}.mdx` | IN7.11–IN7.13 |
| IN7.15 Index and records | as part 1's IN7.8 lists, plus `docs/plans/in-integrations/00-design.md` | IN7.14; part 1's IN7.8 (which records D-IN-28 in `mvp-decisions.md` first) |

**Parallel runs:**
- IN7.9, IN7.10 and IN7.11 can run at the same time: their files don't overlap. IN7.10 and IN7.11 are both Rust phases: they may be edited in parallel, but compile and test one after the other.
- IN7.11 → IN7.12 in order (same files, including the generated CLI reference).
- IN7.13 edits `tools/capture/src/mock.rs` and `scenario.rs`, which no other phase of this plan touches.
- IN7.13 starts after IN7.9, IN7.10, IN7.11 and part 1's IN7.5 (it uses IN7.5's `homeDirs`, `repoFiles` and absolute `bin` in `agents.ts`). It may run beside IN7.12.
- Machine load: one Rust compile at a time. Each Rust phase compiles and tests once, right before its commit.

---

## IN7.9 — Installs

**Goal:** the pinned builds of OpenCode (both), Goose, Oh My Pi and Hermes Agent are installed for the capture tool.

**Edit:** as the phase table says.

**Steps.**
1. **Binaries.** In `fetch-tools.sh`, after the `envoy` `fetch`, add two `fetch` calls with the § Versions URLs and checksums:
   - `"$tools_dir/opencode-v2/2.3.0/opencode"`;
   - `"$tools_dir/goose/1.59.2/goose"`.
   Each takes its archive member as the fourth argument. Read the member path with `tar -tzf <download>` first; if an archive holds more than one executable or the binary is not at a fixed path, stop and ask. After the fetch, check the OpenCode binary's sha256 against § Versions and run each binary's `--version`: expected `opencode v2.3.0` and `1.59.2` (as part of the output).
2. **OpenCode 1.x.** Create `tools/capture/opencode/` in the shape of `tools/capture/n8n/`, with `opencode-ai` 1.19.6 (exact). Add `pnpm_tool opencode 1.19.6 opencode-ai/package.json` after the existing `pnpm_tool` lines. If pnpm blocks an install script, add the package to `allowBuilds` in that project's `pnpm-workspace.yaml` and report it. If pnpm refuses a transitive version as younger than 7 days, stop and report it (do not loosen `minimumReleaseAge`). Then run `<modules>/.bin/opencode --version` (expected `1.19.6`), where `<modules>` is `target/capture-tools/opencode/1.19.6/node_modules`, and check the sha256 of the binary that `find <modules> -path '*opencode-linux-x64/bin/opencode' -type f` finds against § Versions.
3. **Oh My Pi.** Create `tools/capture/oh-my-pi/` the same way, with `@oh-my-pi/pi-coding-agent` 18.22.0 and `bun` 1.4.4 (both exact). Add `pnpm_tool oh-my-pi 18.22.0 @oh-my-pi/pi-coding-agent/package.json`. Bun's binary comes through its install script or its platform package; allow what pnpm names and report it. Run `<modules>/.bin/omp --version` with `<modules>/.bin` first on `PATH`: expected `omp/18.22.0`.
4. **Hermes Agent.** Create `tools/capture/hermes/pyproject.toml` in the shape of `tools/capture/litellm-proxy/pyproject.toml`:
   - `name = "cetra-capture-hermes"`, `package = false`, `exclude-newer = "7 days"` with the same comment;
   - `requires-python`: the range `hermes-agent`'s own `pyproject.toml` declares at the tag, narrowed to one minor version, as `litellm-proxy` does (`==3.X.*`). The spike ran Python 3.14;
   - `dependencies = ["hermes-agent", "hermes-otel"]`, with `[tool.uv.sources]` giving each its git URL and `tag` from § Versions.
   - Run `uv lock --project tools/capture/hermes`. If a dependency cannot resolve under `exclude-newer`, stop and report it.
   - Add a block to `fetch-tools.sh` in the shape of the LiteLLM one: environment `$tools_dir/hermes/0.23.2`, lock copy `$tools_dir/hermes/uv.lock`, check file `bin/hermes`.
   - Then run `$tools_dir/hermes/0.23.2/bin/hermes --version` and report its output. Its version line is not known in advance; record it exactly.
5. Run `just capture-tools` twice. Record each version and publish time.

**Done when:** `just capture-tools` succeeds twice (the second run skips every new tool), the four known `--version` outputs match, and Hermes's is recorded.

**Commit:** `Pin OpenCode, Goose, Oh My Pi and Hermes Agent for captures`

## IN7.10 — Detection

**Goal:** cetra-otel detects OpenCode, Goose, Oh My Pi and Hermes Agent and applies § Rules.

**Edit:** as the phase table says.

**Steps.**
1. `CodingAgent` gains `OpenCode`, `Goose`, `OhMyPi`, `Hermes`; `wire_name` gives `"opencode"`, `"goose"`, `"oh-my-pi"`, `"hermes-agent"`. Update the module doc of `mod.rs` if it lists the agents.
2. `detect_agent`: after the OpenClaw check, four exact scope checks, in this order: `cetra-opencode` → `OpenCode`; `goose` → `Goose`; `@oh-my-pi/pi-agent-core` → `OhMyPi`; `hermes-otel-plugin` → `Hermes`.
3. **One rename for the GenAI-shaped agents, no forwarding modules.** Pi, OpenCode and Oh My Pi all rename `execute_tool <name>` spans. In `mod.rs`:
   ```rust
   /// The prefix of the GenAI tool spans (`execute_tool <name>`) that Pi, OpenCode and Oh My Pi
   /// rename after their tool (D-CA-6).
   const EXECUTE_TOOL_PREFIX: &str = "execute_tool ";

   /// A span whose name starts with `prefix` takes the tool's name (D-CA-6).
   fn rename_prefixed_tool(span_name: &str, prefix: &str, inputs: &Finish, built: &mut Span)
   ```
   Its body is `pi::finish`'s, with `prefix` for `TOOL_PREFIX`. Then:
   - delete `crates/otel/src/agents/pi.rs` and its `mod pi;` line;
   - move its test `pi_tool_span_is_renamed_and_kinds_are_generic`, unchanged, into a new test-only module `crates/otel/src/agents/generic_agent_tests.rs` (`#[cfg(test)] mod generic_agent_tests;` in `mod.rs`), with the module doc "Pi, OpenCode and Oh My Pi: GenAI-shaped agents with no rules of their own beyond the tool rename";
   - no `opencode.rs` or `oh_my_pi.rs` in cetra-otel.
4. New modules, each with a module doc naming D-IN-29 and its spec 02 section:
   - `goose.rs`: `kind(span_name) -> Option<SpanKind>` gives `Agent` for `reply`, `Chain` for `chat`, `Llm` for `stream_response_from_provider`, `None` otherwise. `finish(span_name, inputs, built)` renames the span named exactly `execute_tool` with `rename_tool`.
   - `hermes.rs`: `kind(span_name)` gives `Agent` for `agent`, `Chain` for a name starting `llm.`, `None` otherwise. `finish` calls `rename_prefixed_tool(span_name, "tool.", inputs, built)`.
5. `mod.rs` match arms, each new variant stated explicitly (no wildcard):
   - `discarded`: `false` for all four;
   - `finish_inputs`: the tool name from `gen_ai.tool.name`, as Pi's arm;
   - `kind`: `goose::kind` and `hermes::kind`; `None` for OpenCode and Oh My Pi;
   - `context`: nothing beyond `CODING_AGENT`, as Pi;
   - `usage`, `ttft`: `None`;
   - `payload`: an explicit no-op arm for the four, with the comment "the generic GenAI reading reads their messages" (`gemini_cli::payload` only acts on Gemini's span names);
   - `finish`: `Pi | OpenCode | OhMyPi` → `rename_prefixed_tool(span_name, EXECUTE_TOOL_PREFIX, inputs, built)`; `Goose` → `goose::finish`; `Hermes` → `hermes::finish`.
6. `proptest.rs`: add the four scopes to `SCOPE_GATES`.
7. Spec 02 § Coding agents (`docs/spec/02-ingestion.md`; line numbers as of this plan):
   - the intro list of agents (about line 309);
   - the sentence that Pi discards nothing (about line 332): Pi, OpenCode, Oh My Pi, Goose and Hermes discard nothing;
   - the usage row "Gemini CLI, Copilot, Qwen Code, Pi, OpenClaw | none | generic" (about line 360): add the four;
   - the Names list (about line 378): OpenCode and Oh My Pi `execute_tool *`, Goose `execute_tool`, Hermes `tool.*`;
   - the `coding_agent` value list (about line 390): add `opencode`, `goose`, `oh-my-pi`, `hermes-agent`;
   - the detection list: the four rules;
   - the kind tables: Goose's and Hermes's (§ Rules), and OpenCode and Oh My Pi in the generic row with Pi;
   - the payload sentence: the four use the generic reading;
   - the list of captured agents: add the four;
   - one sentence: OpenCode's spans come from cetra's own plugin (D-IN-29 item 1);
   - one sentence: a wrapper span is `chain`, not dropped, so its children keep their parent.

**Tests.** Every span below has scope as stated and these attributes unless the case says otherwise: `gen_ai.provider.name = "openai"`, `gen_ai.request.model = "gpt-4o-mini"`. Use `test_support`'s `span`, `kv`, `s`, `i`, `request`, `run` and, where cost is checked, `run_priced` with this row (the `claude_code.rs` pattern):
```rust
PriceRow {
    scope: PriceScope::Builtin,
    model_name: "gpt-4o-mini".to_owned(),
    match_pattern: "gpt-4o-mini".to_owned(),
    provider: None,
    valid_from: 0,
    price_input: Some(0.15),
    price_output: Some(0.6),
    price_cache_read: None,
    price_cache_write: None,
    price_reasoning: None,
    tiers: None,
}
```
- In `mod.rs`'s tests:
  - `detect_agent_reads_each_signal` gains the four scopes, plus near misses `cetra-opencode-x`, `goose.x`, `@oh-my-pi/pi-agent`, `hermes-otel`, each giving no agent.
  - `wire_names_are_the_d_ca_4_values` covers the four names.
- In `generic_agent_tests.rs`:
  - the moved Pi test, unchanged;
  - `opencode_tool_span_is_renamed_and_kinds_are_generic` and `oh_my_pi_tool_span_is_renamed_and_kinds_are_generic`: the Pi test's three spans (`invoke_agent …` with `gen_ai.operation.name = "invoke_agent"`; `chat gpt-4o-mini` with `chat`; `execute_tool read` with `execute_tool` and `gen_ai.tool.name = "read"`), on scope `cetra-opencode` and `@oh-my-pi/pi-agent-core`; expect agent, llm, and a tool named `read` with `cetra.span_name = "execute_tool read"`, and `metadata['coding_agent']` `opencode` / `oh-my-pi`.
- In `goose.rs`, scope `goose`, one trace:
  - `reply` (span id `0x11…`, no parent) with `gen_ai.operation.name = "invoke_agent"`, `gen_ai.usage.input_tokens = 213`, `gen_ai.usage.output_tokens = 29`;
  - `chat` (id `0x22…`, parent `reply`) with `gen_ai.operation.name = "chat"`, input 82, output 17;
  - `stream_response_from_provider` (id `0x33…`, parent `chat`) with the same operation and tokens;
  - `execute_tool` (id `0x44…`, parent `reply`) with `gen_ai.operation.name = "execute_tool"`, `gen_ai.tool.name = "shell"`.
  - `goose_rules` (with `run_priced`): `reply` is agent with no token columns; `chat` is chain with no token columns; `stream_response_from_provider` is llm with 82 and 17, cost source `computed`; the tool is named `shell` with `cetra.span_name = "execute_tool"`; the sum of input tokens over all four spans is 82.
  - `goose_names_on_another_scope_are_untouched`: the `chat` span on scope `x` stays llm.
- In `hermes.rs`, scope `hermes-otel-plugin`: `agent` (root); `llm.gpt-4o-mini` (parent `agent`, no tokens); `api.gpt-4o-mini` (parent `llm.gpt-4o-mini`, `gen_ai.operation.name = "chat"`, input 82, output 17); `tool.read_file` (parent `llm.gpt-4o-mini`, `gen_ai.tool.name = "read_file"`).
  - `hermes_rules`: agent, chain, llm with 82 and 17, tool named `read_file` with `cetra.span_name = "tool.read_file"`; every span keeps its parent id.
  - `hermes_names_on_another_scope_are_untouched`: `llm.gpt-4o-mini` on scope `x` keeps its generic kind.

**Done when:**
- `cargo nextest run -p cetra-otel`, clippy (`-D warnings`) and `cargo fmt -p cetra-otel --check` pass.
- No `expected.json` changes (stop if one does).

**Commit:** `Detect OpenCode, Goose, Oh My Pi and Hermes Agent`

## IN7.11 — OpenCode setup and plugin

**Goal:** `cetra agents setup opencode` writes cetra's OpenCode plugin and its config file. `cetra agents remove opencode` removes both.

**Edit:** as the phase table says.

**Steps.**
1. In `mod.rs`, `Agent` gains `OpenCode` (`#[value(name = "opencode")]`, `wire_name` `"opencode"`), with the doc comment "OpenCode: cetra's plugin and its config file are written by setup; no hook (D-IN-29)". Update `mod.rs`'s module doc if it lists the agents.
2. **`opencode_plugin.ts`.** Start from the spike's prototype, `docs/plans/in-integrations/spikes/m7-files/cetra-opencode-prototype.ts`. The plan's text wins where they differ.
   - **First line:** the marker of detail 3, exactly.
   - **Imports:** exactly one, `import { readFileSync } from "node:fs";`. It uses the globals `fetch`, `crypto.getRandomValues`, `AbortSignal` and `process.env`.
   - **No I/O at module load.** The top level only defines constants and functions. The config is read when OpenCode calls `setup` (v2) or `server` (v1), so a test that imports the file reads nothing.
   - **Helpers, exported by name for the Node test**, with these contracts:
     ```ts
     type Attr = { key: string; value: { stringValue?: string; intValue?: string; arrayValue?: { values: { stringValue: string }[] } } };
     export type Config = { endpoint: string; headers: Record<string, string>; captureContent: boolean };

     /** `<XDG_CONFIG_HOME or HOME/.config>/opencode/cetra-opencode.json`, or null when neither variable is set and non-empty. */
     export function configPath(env: Record<string, string | undefined>): string | null;
     /** null unless `text` is a JSON object whose `endpoint`, trimmed, is a non-empty string. */
     export function parseConfig(text: string): Config | null;
     /** OpenCode's token counts as GenAI attributes (input includes cache). */
     export function usageAttributes(tokens: { input?: number; output?: number; reasoning?: number; cache?: { read?: number; write?: number } } | undefined): Attr[];
     /** `tool-calls` → `tool_calls`, `content-filter` → `content_filter`; other strings unchanged; undefined → undefined. */
     export function finishReason(raw: string | undefined): string | undefined;
     ```
     - `parseConfig`: `endpoint` loses trailing `/`; `headers` keeps only string values (absent → `{}`); `captureContent` is `true` only when `capture_content` is the boolean `true`.
     - `usageAttributes`: undefined → `[]`. Otherwise, with each missing count as `0`, in this order: `gen_ai.usage.input_tokens` = `input + cache.read + cache.write`, `gen_ai.usage.output_tokens` = `output`, `gen_ai.usage.reasoning.output_tokens` = `reasoning`, `gen_ai.usage.cache_read.input_tokens` = `cache.read`, `gen_ai.usage.cache_creation.input_tokens` = `cache.write`, each as `{ intValue: String(n) }`. Both the v1 and the v2 paths call it (the prototype's v1 path reads `m.tokens.cache.read` directly, which throws when `cache` is missing: replace it).
     - `gen_ai.response.finish_reasons` is an OTLP `arrayValue` of one string, as `pi_extension.ts` writes it (a string array, as GenAI semconv types it); it is omitted when `finishReason` returns undefined.
   - **Where the config lives.** A module-level `let config: Config | null = null;`, set by `setup` or `server`. The prototype's `endpoint()`, `headers()` and `capture()` read it instead of the environment. Drop the prototype's header comment that names `OTEL_EXPORTER_OTLP_ENDPOINT` (the test below forbids the string); say instead that the plugin reads its own config file.
   - **Start-up.** `setup` (v2) and `server` (v1) each call `configPath(process.env)`, read the file with `readFileSync(path, "utf8")` inside `try`, and `parseConfig` it. When the path is null, the file is missing or unreadable, or `parseConfig` returns null, the plugin registers nothing: v2's `setup` returns `async () => {}`, v1's `server` returns `{}`. OpenCode then runs unchanged.
   - **It reads no environment variable** for the endpoint, headers or content (D-IN-29 item 3). Remove the prototype's `CETRA_OTLP_*` and `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` reads; `env` and `decode` go.
   - **Export:** POST OTLP/JSON to `<endpoint>/v1/traces` with `Content-Type: application/json` plus the config's headers, `AbortSignal.timeout(5000)`. A failed POST is dropped silently; nothing is written to stdout or stderr.
   - **Resource and scope:** `service.name` `opencode`; scope name `cetra-opencode`, version the constant `PLUGIN_VERSION = "1"`.
   - **Spans:** as the prototype (spike § 6.1): `invoke_agent` per turn, `chat <model>` per model call, `execute_tool <name>` per tool call, children of the turn, `gen_ai.conversation.id` = the session id.
   - **Content:** only when `captureContent` is true: `gen_ai.input.messages` (v2 only, from the `context` hook, sent as the prototype sends them), `gen_ai.output.messages`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`.
   - **Default export:** `{ id: "cetra-opencode", setup, server }`, as the prototype (rename `setupV2`/`serverV1` or keep them; they need not be exported by name).
   - **Never throw** from a hook or event handler: each body is wrapped in `try`/`catch` that swallows the error.
3. **`opencode.rs`**, in `pi.rs`'s shape (`crates/cli/src/agents/pi.rs`):
   - `opencode_dir(paths)`: detail 2, with the variable read through `opencode_dir_from(value: Option<OsString>, paths)` so tests never set it, as `pi_dir_from`.
   - `const PLUGIN: &str = include_str!("opencode_plugin.ts")`, `const MARKER` (its first line), `const MANAGED_BY: &str = "cetra agents setup opencode"`, `const PLUGIN_FILE: &str = "cetra-opencode.ts"`, `const CONFIG_FILE: &str = "cetra-opencode.json"`.
   - `config_bytes(url, key, capture) -> anyhow::Result<Vec<u8>>`: detail 3's object as a `serde_json::Map` (insert the keys in detail 3's order; the workspace's `serde_json` has `preserve_order`, so the output keeps it), serialized with `settings_bytes` (`setup.rs`; make it `pub(super)`).
   - `setup(args, url, paths, out)`:
     - If the plugin file exists without the marker, or the config file exists and is not a JSON object whose `managed_by` equals `MANAGED_BY`, fail with "`<path>` exists and was not written by cetra; move it away and run setup again" and write nothing.
     - Write the config, then the plugin, each with `emit_owned` (honours `--print`). Every file setup writes is created mode 0600 (`write_private_file`, `credentials.rs`); the test checks the config's mode.
     - Print: "OpenCode loads the plugin at its next start. Both the npm build (1.x) and the v2 build are supported."
   - `remove(paths, out)`, for the plugin, then the config:
     - absent: "no cetra OpenCode plugin found at `<path>`" / "no cetra OpenCode config found at `<path>`";
     - ours: delete with `fs::remove_file` and print `removed <path>`;
     - not ours: `<path> was not written by cetra; left in place`.
4. **`setup.rs`:** dispatch `Agent::OpenCode`; the `agents_file` condition becomes `!matches!(args.agent, Agent::Copilot | Agent::Pi | Agent::OpenCode)` (detail 4); `hook_timeout(Agent::OpenCode)` is `None`, in the explicit list. Every `match` on `Agent` states the new arm.
5. **`main.rs`:** the `Agents` doc string lists the agents as the code now has them.
6. **Generated page:** run the CLI tests once with `CETRA_DOCS_WRITE=1` so `cli_reference_is_up_to_date` rewrites `website/src/content/docs/docs/reference/cli.md`, and commit it in this phase's commit.

**Tests** (in `opencode.rs`, with `paths_under` as `pi.rs`'s tests):
- `opencode_setup_writes_the_plugin_and_the_config`: the plugin equals `PLUGIN`; the config parses to detail 3's object; its mode is 0600 (`#[cfg(unix)]`).
- `opencode_setup_uses_xdg_config_home`.
- `opencode_no_content_sets_capture_false`.
- `opencode_setup_refuses_a_foreign_plugin` and `opencode_setup_refuses_a_foreign_config`: nothing written, an error returned.
- `opencode_setup_twice_is_idempotent`.
- `opencode_print_writes_nothing`.
- `opencode_remove_deletes_only_our_files`: ours, foreign and absent, for each file, with the exact messages above.
- `opencode_plugin_imports_only_node_fs`: the only `import` line is `import { readFileSync } from "node:fs";`, and there is no `require(`.
- `opencode_plugin_scope_is_the_detection_key`: the file contains `"cetra-opencode"`.
- `opencode_plugin_reads_no_otel_endpoint`: the file does not contain `OTEL_EXPORTER_OTLP`.
- `opencode_plugin.test.mjs` (`node --test`, importing the `.ts` file through Node's type stripping, as `pi_extension.test.mjs`):
  - `configPath`: `XDG_CONFIG_HOME=/x` → `/x/opencode/cetra-opencode.json`; empty `XDG_CONFIG_HOME` and `HOME=/h` → `/h/.config/opencode/cetra-opencode.json`; neither → `null`;
  - `parseConfig`: detail 3's object → `{endpoint, headers: {Authorization: "Bearer k"}, captureContent: true}`; no `endpoint`; `endpoint` `"  "`; non-JSON; a JSON array; a numeric header dropped; `capture_content: "true"` (a string) → `false`; `https://x/` → `https://x`;
  - `usageAttributes`: `{input: 100, output: 20, reasoning: 5, cache: {read: 30, write: 10}}` → the five attributes with `"140"`, `"20"`, `"5"`, `"30"`, `"10"`; `{input: 100, output: 20}` → input `"100"`, cache `"0"`; `undefined` → `[]`;
  - `finishReason`: `tool-calls`, `content-filter`, `stop`, `other`, `undefined`.
- `opencode_plugin_node_tests_pass`: a Rust `#[test]` that runs `node --test` on that file, skipping with a message when `node` is not on `PATH`, as `pi_extension_node_tests_pass`.

**Done when:**
- `cargo nextest run -p cetra-cli` (which includes `cli_reference_is_up_to_date`), clippy and `cargo fmt -p cetra-cli --check` pass, with the regenerated page committed.

**Commit:** `Add cetra agents setup opencode with a cetra OpenCode plugin`

## IN7.12 — Goose and Oh My Pi setup

**Goal:** `cetra agents setup goose` prints Goose's environment lines; `cetra agents setup oh-my-pi` writes Oh My Pi's `.env` lines. `remove` for each.

**Edit:** as the phase table says.

**Steps.**
1. `Agent` gains `Goose` (`#[value(name = "goose")]`, wire `"goose"`, doc "Goose: setup prints its environment lines; no hook (D-IN-29)") and `OhMyPi` (`#[value(name = "oh-my-pi")]`, wire `"oh-my-pi"`, doc "Oh My Pi: setup writes its .env lines; no hook (D-IN-29)"). `hook_timeout` is `None` for both, in the explicit list. Update `mod.rs`'s module doc if it lists the agents.
2. **`goose.rs`:**
   - `setup(args, url, out)`: writes nothing, with or without `--print`. Prints "Goose reads its endpoint, key and content switch from the environment. Add these lines to your shell profile:", then detail 5's lines, each `export NAME=` followed by the value through `shell_quote(Path::new(value))`, exactly as `pi.rs` prints its lines.
   - `remove(out)`: prints "nothing to remove: cetra agents setup goose writes no file. Delete the OTEL_* lines it printed from your shell profile."
3. **`.env` editing in `setup.rs`.** Replace `env_with_header(text, line)` with:
   ```rust
   /// The `.env` text with its `name` line set to `line`, and whether an existing `name` line
   /// with another value was replaced. The first line starting with `<name>=` or
   /// `export <name>=` is replaced and keeps its `\r`; otherwise `line` is appended. Other
   /// lines keep their bytes and order.
   fn env_with_var(text: &str, name: &str, line: &str) -> (String, bool)
   ```
   The two prefixes are built with `format!("{name}=")` and `format!("export {name}=")`. The Gemini and Qwen caller passes `"OTEL_EXPORTER_OTLP_HEADERS"`. Delete `EXPORTED_HEADERS_PREFIX`, which no longer has a user; `HEADERS_PREFIX` stays (`otlp_header_line` uses it). Make `env_with_var`, `env_without_line` and `write_settings` `pub(super)` for `oh_my_pi.rs` (`updated` already is). The Gemini and Qwen tests pass unchanged.
4. **`oh_my_pi.rs`:**
   - `omp_env_path(paths)` = `<home>/.omp/agent/.env`.
   - `omp_lines(url, key, capture) -> Vec<(&'static str, String)>`: detail 6's `(name, line)` pairs, in order.
   - `setup(args, url, paths, out)`: read the file (`read_optional_text`, absent → empty), apply `env_with_var` for each pair, then `emit` the result (a backup is kept: the file may be the user's). When any call replaced a line with another value, print "replaced existing OTEL_* lines in `<path>`". Print "Oh My Pi reads these from its own .env file; no shell profile change is needed."
   - `remove(paths, out)`: load `agents.toml` (`AgentsFile::load(&paths.agents_file())`). Branches, in order, with their exact output:
     1. No `agents.toml`: print `NOT_FOUND_NOTE`; change nothing.
     2. The `.env` is absent: print "nothing to remove".
     3. The `.env` has no line equal to `OTEL_EXPORTER_OTLP_ENDPOINT=<saved url>`: if it has some `OTEL_EXPORTER_OTLP_ENDPOINT=` line, print `OTHER_SERVER_NOTE`, else print "nothing to remove". Change nothing in either case (no line is removed).
     4. Otherwise remove every line equal to a line of `omp_lines(saved url, saved key, true)` (repeated `env_without_line`). When only blank lines remain, delete the file with `fs::remove_file` and print `removed <path>`. Else write the rest with `write_settings(path, bytes, false)`, **without a backup** (a backup of the file as setup left it would keep the key on disk), and print `updated <path>`.
     - Accepted, and written in the function's doc comment: a user's own `OTEL_SERVICE_NAME=omp`, `OTEL_LOGS_EXPORTER=none` or `OTEL_METRICS_EXPORTER=none` line is removed too; and if another agent's setup later saved a different key in the shared `agents.toml`, the old header line stays (as `settings_env_remove` does for Gemini).
5. **`setup.rs`:** dispatch both; the `agents_file` condition skips Goose but not Oh My Pi: `!matches!(args.agent, Agent::Copilot | Agent::Pi | Agent::OpenCode | Agent::Goose)` (detail 4).
6. **`main.rs`:** the `Agents` doc string. **Generated page:** as IN7.11 step 6.

**Tests:**
- `goose_setup_prints_the_lines_and_writes_nothing`: the five lines in order, in the form `pi.rs`'s tests assert, the `%20` header, and no file under the test home.
- `goose_no_content_omits_the_content_line`.
- `goose_remove_prints_the_reminder`.
- `omp_setup_writes_the_env_lines_in_order`.
- `omp_setup_keeps_other_lines_and_replaces_ours`: an existing `.env` with `FOO=1` and `OTEL_EXPORTER_OTLP_ENDPOINT=http://old`; `FOO=1` stays first, the endpoint line is replaced in place, and the note is printed.
- `omp_setup_twice_is_idempotent`.
- `omp_no_content_omits_the_content_line`.
- `omp_print_writes_nothing`.
- `omp_remove_removes_our_lines_and_an_emptied_file`.
- `omp_remove_keeps_other_lines`: `FOO=1` survives, the file stays, and no `*.cetra-backup` file in the directory contains `sk-cetra-`.
- `omp_remove_leaves_another_server`.
- `env_with_var_replaces_an_exported_line_and_keeps_its_carriage_return`.
- The Gemini and Qwen `.env` tests still pass.

**Done when:** `cargo nextest run -p cetra-cli`, clippy and `cargo fmt -p cetra-cli --check` pass, with the regenerated page committed.

**Commit:** `Add cetra agents setup goose and oh-my-pi`

## IN7.13 — Captures

**Goal:** five captured cases from the pinned builds against the mock provider.

**Edit:** as the phase table says.

**Steps.**
1. **Groups:** add `opencode`, `goose`, `oh_my_pi` and `hermes_agent` to `GROUPS` in `tools/capture/src/scenario.rs` and `crates/otel/tests/support/mod.rs`, and to `fixtures/otlp/README.md`. Extend `case_path_accepts_the_coding_agent_groups`.
2. **Mock: auxiliary calls and probes** (`tools/capture/src/mock.rs`, `scenario.rs`). Four of the agents make a small model call besides the main flow, at the same time as the first main call: OpenCode (both builds) and Goose a title or session-name call, Oh My Pi three. The mock hands out turns strictly in arrival order (`Mock::take`), so these calls would take the main flow's turns at random. Two agents also send probe requests the mock now rejects as errors: OpenCode v2 `GET …/v1/responses`; Hermes `POST …/api/show` and `GET …/v1/models/<id>`.
   - `Turn` gains:
     ```rust
     /// Served only to a request that declares no tools: an agent's side call (a title, a
     /// session name) that runs beside the main flow. Default false.
     #[serde(default)]
     pub auxiliary: bool,
     ```
   - `Facts` gains `tools: bool`: the request body has a non-empty `tools` array (all chat routes; embedding routes `false`).
   - `take(route, tools)` replaces the single `next` cursor with two: auxiliary and main, each over its own turns in scenario order. A request without tools takes the next auxiliary turn when one is left, else the next main turn; a request with tools takes the next main turn. "No scripted turn left" and the API check stay as they are, per queue. A scenario without auxiliary turns behaves exactly as today.
   - `served` keeps the turn's scenario index, as today.
   - Tolerated probes, answered without `push_error` (beside the existing `GET …/models`): `GET` on a path ending `/responses` → 404 with the usual error body; `POST` on a path ending `/api/show` → 404; `GET` on a path matching `…/models/<id>` → 200 `{"id": "<id>", "object": "model", "created": 0, "owned_by": "cetra-capture"}`.
   - Scenario validation (`scenario.rs`): an auxiliary turn must be a chat turn with text and no tool calls.
   - Tests (in `mock.rs` and `scenario.rs`, next to the existing ones): `auxiliary_turns_go_to_requests_without_tools` (an auxiliary turn listed first is served to the second request, which has no tools, while the first request, with tools, gets main turn 1); `without_auxiliary_turns_order_is_unchanged`; `unused_auxiliary_turn_follows_the_unused_turn_rule` (an unused auxiliary turn is reported exactly as an unused main turn is today); `responses_get_probe_is_not_an_error`; `api_show_probe_is_not_an_error`; `model_get_returns_the_model`; `auxiliary_turn_with_tool_calls_is_rejected`.
3. **Versions of tools that are not npm packages.** `writeMeta` (`tools/capture/ts/harness.ts`) reads each library's version from `node_modules/<alias>/package.json`. Two agents are release binaries and one is a Python environment, so add a second parameter:
   ```ts
   export function writeMeta(aliases: Record<string, string> = {}, versions: Record<string, string> = {}): void
   ```
   A listed library named in `versions` takes that version and its `package.json` is not read. Other libraries are read as today. Existing callers do not change.
4. **Metadata per case** (`scenario.json`'s `instrumentation` and `libraries`; `instrumentation` must be one of `libraries`):

   | Case | `instrumentation` | `libraries` | How `main.ts` gets the versions |
   |---|---|---|---|
   | `opencode/read-file` | `opencode` | `["opencode"]` | `versions: { opencode: "2.3.0" }` |
   | `opencode/read-file-v1` | `opencode-ai` | `["opencode-ai"]` | alias `{ "opencode-ai": "../../../../target/capture-tools/opencode/1.19.6/node_modules/opencode-ai" }` |
   | `goose/read-file` | `goose` | `["goose"]` | `versions: { goose: "1.59.2" }` |
   | `oh_my_pi/read-file` | `@oh-my-pi/pi-coding-agent` | `["@oh-my-pi/pi-coding-agent"]` | alias `{ "@oh-my-pi/pi-coding-agent": "../../../../target/capture-tools/oh-my-pi/18.22.0/node_modules/@oh-my-pi/pi-coding-agent" }` |
   | `hermes_agent/read-file` | `hermes-otel` | `["hermes-agent", "hermes-otel"]` | `versions: { "hermes-agent": "0.23.2", "hermes-otel": "1.33.0" }` |

   The literal versions are the § Versions pins; keep each in one constant in its `main.ts`. Each `scenario.json` also has `"description"` (one sentence: the agent, its version, the read and the answer) and `"spec_refs": ["02 § Coding agents"]`; set `"unordered_spans": true` if `--check` shows the span order varies between runs (likely for Oh My Pi and Hermes).
5. **Turns.** Every case has two turns, in the shape of `tools/capture/ts/scenarios/gemini_cli/shell-tool/scenario.json`, model `gpt-4o-mini`:
   - turn 1: `message.text` null, one tool call `{ "id": "call_capture_1", "name": <tool>, "arguments": <args> }`, `finish` `tool_calls`, usage input 82, output 17 (others 0);
   - turn 2: `message.text` `"hello.txt says: hello from cetra"`, no tool calls, `finish` `stop`, usage input 131, output 12.

   | Case | `api` | `<tool>` | `<args>` |
   |---|---|---|---|
   | `opencode/read-file` | `openai.responses` | `read` | `{ "path": "hello.txt" }` |
   | `opencode/read-file-v1` | `openai.responses` | `read` | `{ "filePath": "hello.txt" }` |
   | `goose/read-file` | `openai.chat` | `shell` | `{ "command": "cat hello.txt" }` |
   | `oh_my_pi/read-file` | `openai.chat` | `read` | `{ "path": "hello.txt" }` |
   | `hermes_agent/read-file` | `openai.chat` | `read_file` | `{ "path": "hello.txt" }` |

   Auxiliary turns come first in the list, each `"auxiliary": true`, model `gpt-4o-mini`, `message.text` `"Reading a file"`, no tool calls, `finish` `stop`, usage input 20, output 5: one for each OpenCode case and for Goose, three for Oh My Pi, none for Hermes. If a run fails because a call took the wrong turn (a "turn N: expected …" error, or no turn left), stop and ask.

   The tool names and argument keys are the ones each agent offered in the spikes (Goose's `developer` builtin offers `shell`, `edit`, `tree`, `write`, `read_image`, `load_skill`; it has no read tool). The path is relative to the repo, which is each agent's working directory. If a tool span ends with an error status, stop and ask (an absolute path cannot be used: it differs per machine).
6. **Endpoints.** `CETRA_CAPTURE_OTLP_ENDPOINT` ends in `/v1/traces` (`tools/capture/src/run.rs`). As `gemini_cli/shell-tool/main.ts` does, strip that suffix to get the base URL. OpenCode's config `endpoint`, Goose's and Oh My Pi's `OTEL_EXPORTER_OTLP_ENDPOINT` take the **base** URL (each appends `/v1/traces` itself); Hermes's backend `endpoint` takes the **full** URL. The receiver accepts and discards POSTs to `/v1/logs` and `/v1/metrics` (`receiver.rs`), so Hermes's logs and metrics need no switch. The mock key is `requiredEnv("OPENAI_API_KEY")` (`capture-mock-key`, `run.rs`); the OTLP header value is `Bearer ` plus the same key.
7. **Scenarios.** Each is a `read-file` case in the shape of part 1's `pi/read-file`: `hello.txt` (content `hello from cetra\n`) in the repo through `repoFiles`. Every binary path is absolute under `process.env.CETRA_CAPTURE_TOOLS`. `<mock>` is the mock provider's base URL as the gemini scenario gets it; `<key>` the mock key of step 6; `<base>` and `<traces>` as step 5; `<home>` from `agentPaths()`.
   - **`opencode/read-file`** (v2):
     - bin `opencode-v2/2.3.0/opencode`; args `["run", "--standalone", "--auto", "<prompt>"]`;
     - environment: `OPENCODE_DISABLE_AUTOUPDATE=1`, `OPENCODE_DISABLE_MODELS_FETCH=1`, `OPENCODE_DISABLE_LSP_DOWNLOAD=1`, `OPENCODE_DISABLE_DEFAULT_PLUGINS=1`, `OPENAI_API_KEY=<key>`, `XDG_CONFIG_HOME=<home>/.config`. No `OTEL_*` variable;
     - `homeFiles`:
       - `.config/opencode/plugins/cetra-opencode.ts`: the content of `crates/cli/src/agents/opencode_plugin.ts`, read at run time from `path.join(<repo root>, "crates/cli/src/agents/opencode_plugin.ts")`, where the repo root is `path.resolve(<this main.ts's directory>, "../../../../../..")` (check it resolves to the directory holding `Cargo.toml`);
       - `.config/opencode/cetra-opencode.json`: detail 3 with `endpoint` `<base>`, header `Bearer <key>`, `capture_content` `true`;
     - `repoFiles`: `opencode.json` = `{"$schema":"https://opencode.ai/config.json","model":"openai/gpt-4o-mini","providers":{"openai":{"settings":{"baseURL":"<mock>/v1"}}}}`.
   - **`opencode/read-file-v1`:** the same, except bin `opencode/1.19.6/node_modules/.bin/opencode`, args `["run", "--auto", "<prompt>"]`, and `opencode.json` = `{"$schema":"https://opencode.ai/config.json","provider":{"openai":{"options":{"baseURL":"<mock>/v1","apiKey":"<key>"}}},"model":"openai/gpt-4o-mini"}`.
   - **`goose/read-file`:**
     - bin `goose/1.59.2/goose`; args `["run", "-t", "<prompt>", "--no-session", "--no-profile", "--with-builtin", "developer", "-q", "--output-format", "text"]`;
     - `homeFiles`: `.config/goose/config.yaml` with exactly these three lines: `GOOSE_PROVIDER: openai`, `GOOSE_MODEL: gpt-4o-mini`, `GOOSE_MODE: auto`;
     - environment: `XDG_CONFIG_HOME=<home>/.config`, `OPENAI_API_KEY=<key>`, `OPENAI_BASE_URL=<mock>/v1`, `GOOSE_PROVIDER=openai`, `GOOSE_MODEL=gpt-4o-mini`, `GOOSE_MODE=auto`, `OTEL_EXPORTER_OTLP_ENDPOINT=<base>`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`, `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`, `OTEL_LOGS_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none`.
   - **`oh_my_pi/read-file`:**
     - bin `oh-my-pi/18.22.0/node_modules/.bin/omp`; args `["-p", "--auto-approve", "--model", "openai/gpt-4o-mini", "<prompt>"]`;
     - environment: `PATH` = `<CETRA_CAPTURE_TOOLS>/oh-my-pi/18.22.0/node_modules/.bin` + `:` + `process.env.PATH` (Bun must be found; `run.env` is applied after the runner's `PATH`);
     - `homeFiles`:
       - `.omp/agent/models.yml`:
         ```yaml
         providers:
           openai:
             baseUrl: <mock>/v1
             api: openai-completions
             apiKey: <key>
             models:
               - id: gpt-4o-mini
                 name: gpt-4o-mini
         modelRoles:
           default: openai/gpt-4o-mini
         ```
       - `.omp/agent/.env`: detail 6's six lines with `<base>` and `<key>`.
   - **`hermes_agent/read-file`:**
     - bin `hermes/0.23.2/bin/hermes`; args `["chat", "-q", "<prompt>", "--oneshot", "--yolo", "--provider", "custom", "--base-url", "<mock>/v1", "--api-key", "<key>", "-m", "gpt-4o-mini"]`;
     - environment: `HERMES_HOME=<home>/.hermes`;
     - `homeFiles`:
       - `.hermes/config.yaml`:
         ```yaml
         model:
           provider: "custom"
           base_url: "<mock>/v1"
           api_key: "<key>"
           default: "gpt-4o-mini"
           streaming: true
         agent:
           toolsets: ["base"]
         display:
           telemetry: false
         plugins:
           enabled: [hermes_otel]
         ```
       - `.hermes/hermes_otel.yaml`:
         ```yaml
         project_name: cetra-capture
         backends:
           - type: otlp
             endpoint: <traces>
             headers:
               Authorization: "Bearer <key>"
             metrics: false
         content_capture: full
         capture_logs: false
         ```
       (`metrics` is a per-backend key of `hermes-otel`, `hermes_otel/backend_config.py`.)
     - **Exit.** In the spike Hermes printed its answer and then kept running (its exporters kept posting) until the runner killed it. `runAgent` needs exit 0 within 90 s. If Hermes does not exit on its own with this config, stop and ask; do not change `agents.ts`.
     - Hermes probes the provider (`POST …/api/show`, `GET …/v1/models/gpt-4o-mini`, `GET …/api/v1/models`); step 2 makes the mock tolerate them. If another probe makes the capture fail, stop and ask.
   - The prompt is `Read hello.txt and tell me what it says.` in every case.
   - **Network.** Captures run in the loopback-only namespace (`just _capture-namespace`, D-IN-26 item 11), so no update check or telemetry call leaves the machine. If an agent stalls on a network call, stop and ask.
   - If a run needs anything not listed here (a flag, a file, a variable), stop and ask.
   - Set `volatile_attributes` and `timing_attributes` from what `--check` shows varying between two runs, each with a reason in `notes`.
8. **Capture, check and bless,** as part 1's IN7.5 step 6.
   - **Expected:**
     - **OpenCode v2:** exactly four spans on scope `cetra-opencode` (one plugin load, nothing doubled): agent, llm ×2, tool `read`, `coding_agent` `opencode`; the first `chat` has input messages. The auxiliary call has no span.
     - **OpenCode v1:** the same four spans, kinds and tokens; the `chat` spans have no input messages (D-IN-29 item 4).
     - **Goose:** no span for the auxiliary call unless Goose makes one (record it); agent `reply`, chain `chat` ×2, llm `stream_response_from_provider` ×2 with 82/17 and 131/12, tool `shell`, `coding_agent` `goose`.
     - **Oh My Pi:** agent, llm ×2 under it with 82/17 and 131/12, tool `read`, plus three parentless llm `chat` spans for the auxiliary calls (20/5 each), `coding_agent` `oh-my-pi`. Record the three in the case's `notes` for IN7.14's Limits.
     - **Hermes:** agent, chain `llm.gpt-4o-mini`, llm `api.gpt-4o-mini` ×2, tool `read_file`, `coding_agent` `hermes-agent`, provider `custom` (Hermes's provider name for an OpenAI-compatible endpoint) and cost source `none` (the spike's result; no price row matches provider `custom`).
     - The others: provider `openai`, model `gpt-4o-mini`, tokens as the turns say, cost `computed`.
   - Stop if a case differs in kinds, tokens or provider.

**Done when:**
- the five cases pass `--check`, and the existing captured cases still pass `just check-capture` with the mock change;
- `cargo nextest run -p cetra-otel` and `cargo nextest run -p cetra-capture` pass, with clippy and `cargo fmt --check` on both;
- `tools/capture/ts`'s lint and type check pass (the recipe `check-capture` uses for TypeScript), and its tests if `harness.ts` has any.

**Commit:** `Capture OpenCode, Goose, Oh My Pi and Hermes Agent`

## IN7.14 — Pages

**Goal:** `opencode`, `goose`, `oh-my-pi` and `hermes-agent` pages in the shape of `gemini-cli.mdx`: `<Backing fixtures={[...]} />`, What you get (from `expected.json`), setup, Limits. Every code fence that no test runs (the files, the `.env` and `export` lines, the YAML) gets `{/* untested: <reason> */}`, as part 1's IN7.6.

| Page | Setup | Limits |
|---|---|---|
| `opencode` (Backing both cases) | `cetra agents setup opencode`; the manual route: the two files of detail 3 | both builds supported; 1.x model calls have no input messages and no tool calls in their output; a subagent session is its own trace; no repository or branch; the interactive TUI was not captured; OpenCode 1.x says nothing when a plugin fails to load; do not set `OTEL_EXPORTER_OTLP_ENDPOINT` for OpenCode, which turns on its own exporter (hundreds of internal spans per turn) |
| `goose` | `cetra agents setup goose` and the printed lines | the lines apply to every OpenTelemetry program started from that shell; the model-call span is `stream_response_from_provider`, and `chat` is its `chain` parent |
| `oh-my-pi` | `cetra agents setup oh-my-pi` and the `.env` lines | a project `.env` with `OTEL_*` lines also routes Oh My Pi's telemetry; Oh My Pi's side calls (three per prompt in the capture) appear as separate model calls without a parent; Oh My Pi exports over HTTP/protobuf only |
| `hermes-agent` | install `hermes-otel` into Hermes's environment (`uv pip install "hermes-otel @ git+https://github.com/briancaffey/hermes-otel@hermes-otel-v1.33.0"` or Hermes's own plugin instructions, whichever the package README gives; don't write a command you can't find there), enable the plugin in `config.yaml`, write `hermes_otel.yaml` with cetra's endpoint (`<url>/v1/traces`) and key | `cetra agents setup` does not support Hermes; the telemetry plugin is a community project; with Hermes's `custom` provider the spans show provider `custom` and no cost; Hermes also sends logs (and metrics unless `metrics: false`), which cetra drops |

**Done when:** `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-backing.mjs && node scripts/check-integrations.mjs` pass. If `check-integrations` fails only because the pages are not yet in the index, say so and leave the fix to IN7.15.

**Commit:** `Add OpenCode, Goose, Oh My Pi and Hermes Agent pages`

## IN7.15 — Index and records

**Goal:** list the new agents wherever part 1's IN7.8 listed agents, and record D-IN-29.

**Steps.**
1. Every file of part 1's IN7.8 steps 1 and 2: rows, cards, sidebar entries and agent lists for OpenCode, Goose and Oh My Pi (first-class) and Hermes Agent (captured page).
2. Spec 09: the setup and remove rows for `opencode`, `goose` and `oh-my-pi`.
3. `00-design.md`: the M7 milestone line, "part 1 and part 2 done".
4. `mvp-decisions.md`: D-IN-29 in the Track IN section.
5. CHANGELOG `Added`: the four agents, `cetra agents setup opencode|goose|oh-my-pi`, the four pages.
6. Roadmap: "M7 implemented" and D-IN-29.
7. `just docs-gen`.

**Done when:** `just check-docs` passes; `cd web && pnpm vitest run src/lib/dashboards` passes if `templates.ts` changed.

**Commit:** `List OpenCode, Goose, Oh My Pi and Hermes Agent and record D-IN-29`

## After the last phase

`just check`, a Sonnet code review, fixes by a GLM agent, then a push to PR #242.
