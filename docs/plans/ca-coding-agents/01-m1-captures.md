Status: plan, written 2026-11-20 from the code at `1592a89`.

# CA · M1 — Coding-agent captures

Milestone M1 of track CA. Design: `docs/plans/ca-coding-agents/00-design.md` § Milestones and D-CA-12. Spike results: `docs/research/coding-agents.md` § 6.

M1 makes `tools/capture` record the real Claude Code, Codex and Gemini CLI against its mock provider, and commits their fixture inputs, awaiting bless, in the new groups `claude_code`, `codex` and `gemini_cli`. It also commits one hand-written Copilot case. **Haiku implements it one phase at a time.** If a step is unclear, contradicts the code, or an agent does not behave as written here, **stop and report**; do not improvise.

## Prerequisites

- Branch `ca-m1-captures`, cut from `claude/ca-coding-agents-22e274` (the design PR).
- Run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` in every shell before `pnpm`, `node` or `just check`.
- Load rules: compile and test Rust only once per phase, right before the commit (`cargo test -p cetra-capture` and `cargo test -p cetra-otel --test fixtures`). Do not run `cargo` while iterating.

## Read first

- `AGENTS.md` § Tests (the captured-fixture rules).
- `fixtures/otlp/README.md` (all of it).
- `docs/plans/b2-capture/README.md` § The scenario format.
- `tools/capture/src/scenario.rs`, especially `GROUPS`, `Scenario`, `Scenario::check` and the tests module.
- `tools/capture/src/determinism.rs`: `deterministic`, `merge`, `remint`.
- `tools/capture/src/run.rs`: `program_env`, `run_case`, the call to `deterministic`.
- `tools/capture/src/mock.rs`: `Mock::router`, `unknown`.
- `tools/capture/ts/harness.ts`, `tools/capture/ts/package.json`, and one scenario (`tools/capture/ts/scenarios/vercel/v5-anthropic-cache/`).
- `crates/otel/tests/support/mod.rs`: `GROUPS`.

## What it is, and what it is not

**When M1 is done:**
- `just capture claude_code/bash-tool`, `just capture codex/exec-command` and `just capture gemini_cli/shell-tool` each run the pinned agent CLI headless against the mock and the receiver. Each writes `fixtures/otlp/<group>/<case>/input.json` and `case.json`, with no `expected.json`.
- `just check-capture` recaptures them and finds no difference.
- The golden test lists the three cases as awaiting bless.
- `fixtures/otlp/genai/copilot-cli-tool/` is a hand-written Copilot case. Its `expected.json` is blessed with today's normalizer.

**Not in M1:**
- Any change to `crates/otel/src`: that is M2, which also blesses the three captured cases.
- The hooks, the server and the setup command: M3 and M4.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-12 | Fixtures captured from pinned agent builds against the mock; new groups; `span_names` allowlist; Copilot hand-written in `genai` |
| D114–D118 | Capture tool, mock provider, scrubbing, bless workflow |
| D74 / D117 | No package younger than 7 days (`minimumReleaseAge`) |

New in this plan, settled by the spike:
- **The `volatile_attributes` scenario option.** It re-mints random identifiers so that two captures compare equal. Agents put UUIDs (session, turn) and timestamps (Gemini call ids) in attributes; the SDK scenarios never did.
- **Gemini's `gen_ai.input.messages` and `gen_ai.system_instructions` go in `timing_attributes`.** Their text includes the current date, so `check-capture` must ignore their values. The key name is historical: the list means "values ignored by the check".

## Versions

The newest releases at least 7 days old on 2026-11-20 (`npm view <pkg> time`). Re-check with `npm view`. Use exactly these versions unless one is no longer installable, and report the versions used.

| Package | Version | Where | Why |
|---|---|---|---|
| `@anthropic-ai/claude-code` | `2.1.323` | `tools/capture/ts/package.json` `dependencies` | the agent under test |
| `@openai/codex` | `0.167.2` | same | the agent under test |
| `@google/gemini-cli` | `0.68.0` | same | the agent under test |

## Phases and dependencies

```
C1 (groups, mock route) ──► C2 (scenario options) ──► C3 (TS agent helper + packages) ──► C4 (3 scenarios + captures) ──► C5 (Copilot case)
```

All phases are sequential. C5 touches only `fixtures/otlp/genai/` and could run in parallel with C3 and C4, but it runs last to keep the branch linear.

---

## C1 — Groups and the mock's `HEAD /api/hello`

**Goal:** the three new fixture groups are accepted everywhere, and the mock answers Claude Code's connectivity check.

**Edit:**
- `tools/capture/src/scenario.rs`
- `tools/capture/src/mock.rs`
- `crates/otel/tests/support/mod.rs`
- `fixtures/otlp/README.md`

**Steps:**
1. In `tools/capture/src/scenario.rs`, `GROUPS`: append `"claude_code"`, `"codex"`, `"gemini_cli"`, in that order, after `"google_adk"` and before `"edge"`.
2. In `crates/otel/tests/support/mod.rs`, `GROUPS`: insert the same three names after `"google_adk"`, before `"cetra"`.
3. In `fixtures/otlp/README.md` § Layout, the sentence that lists the group directories becomes:

   > Group directories are `genai`, `openinference`, `openllmetry`, `vercel`, `langfuse`, `laminar`, `google_adk`, `claude_code`, `codex`, `gemini_cli` (coding agents, track CA), `cetra` (spans from our own SDKs), and `edge` (cross-cutting edge cases).

4. In `tools/capture/src/mock.rs`, `Mock::router`: add `.route("/api/hello", axum::routing::head(hello))` before `.fallback(unknown)`, with:

   ```rust
   /// Claude Code's start-up connectivity check (`HEAD /api/hello`). Not a turn and not an error.
   async fn hello() -> StatusCode {
       StatusCode::OK
   }
   ```

   Axum answers `HEAD` with an empty body.

**Tests:**
- `scenario.rs`: `case_path_accepts_the_coding_agent_groups`. Parse `claude_code/x`, `codex/x` and `gemini_cli/x`; all three are `Ok`.
- `mock.rs`: `head_api_hello_is_ok_and_not_logged`. `HEAD /api/hello` gives 200, and `finish()` then has empty `served` and empty `errors`. Use the existing test helpers that send requests to the router.

**Done when:**
- `cargo test -p cetra-capture` passes.
- `cargo test -p cetra-otel --test fixtures` passes.
- `cargo clippy -p cetra-capture --all-targets -- -D warnings` passes.

**Commit:** `Accept the coding-agent fixture groups in the capture tool`

---

## C2 — Scenario options `span_names` and `volatile_attributes`

**Goal:** a scenario can keep only named spans (Codex's ~700 spans must fit in 256 KiB), and can re-mint random attribute values so that recaptures compare equal.

**Edit:**
- `tools/capture/src/scenario.rs`
- `tools/capture/src/determinism.rs`
- `tools/capture/src/run.rs`
- `tools/capture/src/write.rs` (only its test `Scenario` literal)
- `docs/plans/b2-capture/README.md` (§ The scenario format: document both keys)

**Steps:**
1. In `Scenario`, add after `timing_attributes`:

   ```rust
   /// Span names to keep. Empty keeps every span. Applied before the determinism steps.
   #[serde(default)]
   pub span_names: Vec<String>,
   /// Attribute keys whose string values are random per run (UUIDs, timestamped ids).
   /// Their values are replaced by `capture-volatile-<n>` everywhere they appear.
   #[serde(default)]
   pub volatile_attributes: Vec<String>,
   ```

2. In `Scenario::check`, add two rules. Each returns the existing `ScenarioInvalid` reason style, as the `timing_attributes` checks do:
   - every entry of either list is a non-empty string;
   - neither list has duplicates.
3. In `determinism.rs`, change the signature to:

   ```rust
   pub fn deterministic(
       exports: Vec<ExportTraceServiceRequest>,
       case: &CasePath,
       ingest_time: i64,
       timestamps: &[TimestampAttribute],
       span_names: &[String],
       volatile: &[String],
   ) -> Result<ExportTraceServiceRequest, CaptureError>
   ```

   The body becomes: `select(&mut exports, span_names)` (new, first), then `merge`, `remint`, `revolatile(&mut groups, volatile)` (new, after `remint`), `shift`, `prune`.
4. `fn select(exports: &mut [ExportTraceServiceRequest], span_names: &[String])`:
   - With an empty `span_names`, it does nothing.
   - Otherwise, in every scope it keeps only the spans whose `name` is in `span_names`.
   - It does not change parent ids. A kept span whose parent was dropped keeps its parent id. `remint` already mints a new id for a parent that is not in the export (`assign`, around `determinism.rs:247`), and `nest` treats such a span as a root. No code change is needed for that.
5. `fn revolatile(groups: &mut [ResourceGroup], keys: &[String])`:
   1. With an empty `keys`, it does nothing.
   2. **Collect.** Walk the spans in output order: groups, then scopes, then spans, each span's attributes in order, then each event's attributes in order. Collect the string values of attributes whose key is in `keys` into an ordered map `value → "capture-volatile-<n>"`. `n` starts at 1, by first appearance, and an equal value keeps its first number. Resource and scope attributes are walked too, before their spans.
   3. **Exact replace.** Every string attribute value (resource, scope, span, event and link attributes, and strings nested in array and kvlist values) that **equals** a collected value is replaced by its placeholder.
   4. **Substring replace.** Then, in every string value at the same places, each collected value of **at least 16 characters** is replaced as a substring. Replace longest first, as `scrub::replace_text` does.
   5. Non-string values are not touched. Keys match exactly (no prefix match). Codex's `thread.id` is a string (the spike saw `"20"` and a UUID), so it is masked.
6. In `run.rs`, after `deterministic` returns: when `scenario.span_names` is non-empty and the request holds no span, return the existing `CaptureError::CaptureIncomplete`, with the reason "span_names matched no span". Its fields are whatever that variant already has; read them in `error.rs` and do not add a variant. Test: `span_names_that_match_nothing_fail_the_capture`.
6b. `Scenario` has no `Default`, so every struct literal must name the new fields. Add `span_names: Vec::new(), volatile_attributes: Vec::new(),` to the test literal in `write.rs` (about lines 186–196). Run `grep -rn 'Scenario {' tools/capture/src` and update any other literal the same way.
7. In `run.rs`, pass `&scenario.span_names` and `&scenario.volatile_attributes` to `deterministic`. Update every test call of `deterministic` in `determinism.rs` with `&[], &[]`.
8. Document both keys in `docs/plans/b2-capture/README.md` § The scenario format, after `timing_attributes`, one bullet each, in the words of the doc comments above. Add this sentence: "`timing_attributes` also holds attributes whose text depends on the day or the machine (Gemini CLI's prompt context); `just check-capture` ignores their values."

**Tests** (in `determinism.rs` tests, and one in `scenario.rs`):
- `span_names_keep_only_named_spans`: three spans `a`, `b`, `c`, where `b` is the child of `a`. Keep `["b", "c"]`. The output has exactly `b` and `c`. `b.parent_span_id` is 8 non-zero bytes, differs from every kept span id, and is identical between two runs whose original ids differ.
- `empty_span_names_keep_everything`.
- `volatile_values_are_renumbered_by_first_appearance`: two spans whose `session.id` is the same UUID, and a third with another UUID. The placeholders are `capture-volatile-1` (twice) and `capture-volatile-2`.
- `volatile_substrings_are_replaced_in_other_attributes`: a 36-character id in `gen_ai.tool.call_id`, and the same id inside a JSON string in `gen_ai.input.messages`. Both are replaced.
- `short_volatile_values_are_replaced_only_exactly`: `thread.id = "20"` is declared volatile, and another attribute holds `"port 2026"`. That attribute is unchanged.
- `two_runs_with_different_ids_give_the_same_output`: the same spans twice, with different random UUIDs in a volatile key. `deterministic` gives equal requests.
- `scenario.rs`: `span_names_and_volatile_attributes_reject_empty_and_duplicate_entries`.

**Done when:**
- `cargo test -p cetra-capture` passes.
- `cargo clippy -p cetra-capture --all-targets -- -D warnings` passes.

**Commit:** `Add span_names and volatile_attributes to capture scenarios`

---

## C3 — The TypeScript agent helper and the pinned agent packages

**Goal:** scenarios can run an agent CLI with an isolated home, a git repository and an explicit environment, and record the agent's version.

**Edit:**
- `tools/capture/ts/package.json`
- `tools/capture/ts/pnpm-workspace.yaml` (only if step 1 needs it)
- `tools/capture/ts/tsconfig.json` (add `"agents.ts"` to `include`, so the type check covers it)
- `tools/capture/ts/pnpm-lock.yaml` (regenerated by pnpm)
- `tools/capture/ts/harness.ts`
- `tools/capture/ts/agents.ts` (new)

**Steps:**
1. Add to `dependencies` in `tools/capture/ts/package.json`, with exact versions:

   ```json
   "@anthropic-ai/claude-code": "2.1.323",
   "@openai/codex": "0.167.2",
   "@google/gemini-cli": "0.68.0"
   ```

   Then run `pnpm --dir tools/capture/ts install`. If pnpm refuses a package because of `minimumReleaseAge`, stop and report. If it fails because build scripts were ignored (`ERR_PNPM_IGNORED_BUILDS` or similar), add each listed package to `pnpm-workspace.yaml` under the key pnpm's message names (`allowBuilds: {<pkg>: false}` or `ignoredBuiltDependencies`), so that the scripts stay off, and re-run. The spike ran all three agents with no install scripts. The existing `overrides` entry (`@opentelemetry/api: 1.9.0`) also applies to the agents' dependencies. If pnpm reports a conflict with it, stop and report; do not remove it.
2. In `harness.ts`, move the body of `finish` after `provider.shutdown()` into an exported function. `finish` calls it, so its behaviour is unchanged:

   ```ts
   /** Writes `CETRA_CAPTURE_META` with the versions of `CETRA_CAPTURE_LIBRARIES`. */
   export function writeMeta(aliases: Record<string, string> = {}): void
   ```

3. Create `tools/capture/ts/agents.ts` with exactly this public surface:

   ```ts
   export interface AgentRun {
     /** Executable under node_modules/.bin, e.g. "claude". */
     bin: string;
     args: string[];
     /** The child's whole environment besides PATH, HOME, TERM, LANG and TZ. */
     env: Record<string, string>;
     /** Files to write before the run, relative to the isolated HOME. */
     homeFiles: Record<string, string>;
   }

   /** Paths of one run, for building config files before calling runAgent. */
   export interface AgentPaths {
     home: string;
     repo: string;
   }

   export function agentPaths(): AgentPaths;
   export async function runAgent(run: AgentRun): Promise<void>;
   ```

   `agentPaths()`:
   - Returns `home = <repo root>/target/capture-agents/<case>/home` and `repo = <repo root>/target/capture-agents/<case>/repo`.
   - `<case>` is `CETRA_CAPTURE_CASE` with `/` replaced by `__`.
   - The repo root is `path.resolve(projectRoot, "../../..")`, where `projectRoot` is the directory of `agents.ts`: `fileURLToPath(new URL(".", import.meta.url))`, computed in `agents.ts` itself, since `harness.ts` does not export its own.
   - `agents.ts` uses only erasable TypeScript syntax: no `enum`, no parameter properties, no namespaces. Node runs it by type stripping (`tsconfig.json` `erasableSyntaxOnly`).
   - It throws when `CETRA_CAPTURE_CASE` is unset.

   `runAgent(run)`, in order:
   1. Delete `<repo root>/target/capture-agents/<case>` recursively if it exists, then create `home` and `repo`.
   2. Write each `homeFiles` entry under `home`, creating parent directories. Mode `0o600`, except names ending in `.sh`, which get `0o755`.
   3. In `repo`, run each of these with `execFileSync` and an environment of `PATH`, `HOME = home` and `GIT_CONFIG_NOSYSTEM = "1"`:
      - `git init -q -b feature/capture`;
      - `git config user.email capture@example.com`;
      - `git config user.name capture`;
      - `git remote add origin https://github.com/acme/widgets.git`;
      - `git commit -q --allow-empty -m init`.
   4. Spawn `path.join(projectRoot, "node_modules/.bin", run.bin)` with `run.args`:
      - cwd `repo`;
      - `stdio: ["ignore", 2, "inherit"]`: the agent's stdout goes to this process's stderr, so it never mixes with anything the runner reads;
      - environment exactly `{PATH, HOME: home, TERM: "dumb", LANG: "C.UTF-8", TZ: "UTC", ...run.env}`, where `PATH` is `process.env.PATH` (throw `Error("PATH is unset")` when it is undefined). **Not** `process.env`: the runner sets `OTEL_SERVICE_NAME`, which would rename the agent. `AgentRun.env` is the child's environment, unrelated to `scenario.json`'s `env` key, which stays `{}`.
   5. Kill the child with `SIGTERM` after **90 s**: the runner's own program timeout is 120 s, `PROGRAM_TIMEOUT` in `run.rs`. Wait for the `exit` event before throwing. A timeout, or an exit other than code 0, throws `Error("agent exited with <code or signal>")`. The message never includes the agent's output.
4. Do not change any other scenario.

**Tests:** `tsc --noEmit` through `pnpm --dir tools/capture/ts run typecheck`. The helper is exercised by C4's captures.

**Done when:** `pnpm --dir tools/capture/ts run typecheck` and `pnpm exec biome check tools/capture/ts` pass, and the existing TS scenarios still capture with no change (`just capture vercel/v5-anthropic-cache`, then `git status` shows no change under `fixtures/`).

**Commit:** `Add the agent runner to the TypeScript capture harness`

---

## C4 — The three agent scenarios and their captures

**Goal:** captured inputs for Claude Code, Codex and Gemini CLI.

**Edit:**
- `tools/capture/ts/scenarios/claude_code/bash-tool/{scenario.json,main.ts}` (new)
- `tools/capture/ts/scenarios/codex/exec-command/{scenario.json,main.ts}` (new)
- `tools/capture/ts/scenarios/gemini_cli/shell-tool/{scenario.json,main.ts}` (new)
- `fixtures/otlp/{claude_code/bash-tool,codex/exec-command,gemini_cli/shell-tool}/{input.json,case.json}` (written by the tool)
- `tools/capture/src/run.rs` (the check's comparison only, step 5b)

**The common shape of `main.ts`:**

```ts
import { agentPaths, runAgent } from "../../../agents.ts";
import { writeMeta } from "../../../harness.ts";

async function main(): Promise<void> {
  const { home } = agentPaths();
  await runAgent({ /* per agent, below */ });
  writeMeta();
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
```

`CETRA_CAPTURE_MOCK_URL` (`http://127.0.0.1:<port>`) and `CETRA_CAPTURE_OTLP_ENDPOINT` (`http://127.0.0.1:<port>/v1/traces`) come from `process.env`. Read them with a small `required(name)` that throws when the variable is unset. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `GEMINI_API_KEY` hold the mock key; pass them on explicitly.

### Claude Code: `claude_code/bash-tool`

**`main.ts` `runAgent` argument:**
- `bin: "claude"`.
- `args: ["-p", "Run echo hello with Bash", "--allowedTools", "Bash", "--output-format", "text"]`.
- `homeFiles: {}`.
- `env`:
  - `ANTHROPIC_API_KEY: required("ANTHROPIC_API_KEY")`
  - `ANTHROPIC_BASE_URL: required("CETRA_CAPTURE_MOCK_URL")`
  - `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1"`
  - `DISABLE_AUTOUPDATER: "1"`
  - `CLAUDE_CODE_ENABLE_TELEMETRY: "1"`
  - `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA: "1"`
  - `OTEL_TRACES_EXPORTER: "otlp"`
  - `OTEL_LOGS_EXPORTER: "none"`
  - `OTEL_METRICS_EXPORTER: "none"`
  - `OTEL_EXPORTER_OTLP_PROTOCOL: "http/protobuf"`
  - `OTEL_EXPORTER_OTLP_COMPRESSION: "none"` (the receiver refuses compressed bodies with 415)
  - `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: required("CETRA_CAPTURE_OTLP_ENDPOINT")`
  - `OTEL_TRACES_EXPORT_INTERVAL: "500"`
  - `OTEL_METRICS_INCLUDE_REPOSITORY: "true"`
  - `OTEL_LOG_USER_PROMPTS: "1"`
  - `OTEL_LOG_TOOL_DETAILS: "1"`
  - `OTEL_LOG_TOOL_CONTENT: "1"`

**`scenario.json`:**
- `description`: "Claude Code 2.1.323 in -p mode runs one Bash tool call; beta trace export with prompt and tool content."
- `spec_refs`: `["02 § Convention detection"]` (M2 adds a coding-agents section and updates the refs).
- `instrumentation`: `"@anthropic-ai/claude-code"`.
- `libraries`: `["@anthropic-ai/claude-code"]`.
- `env`: `{}`.
- `timing_attributes`: `["duration_ms", "ttft_ms", "first_content_ms", "interaction.duration_ms"]`.
- `volatile_attributes`: `["session.id", "gen_ai.conversation.id", "user.id"]`.
- `turns`, two of them:
  1. `api: "anthropic.messages"`, `model: "claude-opus-5-5"`, a message with no text and `tool_calls: [{"id": "toolu_capture_1", "name": "Bash", "arguments": {"command": "echo hello", "description": "Print hello"}}]`, `finish: "tool_calls"`, `usage: {input: 420, output: 12, cache_read: 300, cache_write: 20, reasoning: 0}`.
  2. `api: "anthropic.messages"`, `model: "claude-opus-5-5"`, `text: "Done: the command printed hello."`, `finish: "stop"`, `usage: {input: 450, output: 9, cache_read: 300, cache_write: 0, reasoning: 0}`.

  **Turn format** (`scenario.rs` `Turn`, `deny_unknown_fields`): `text` and `tool_calls` live **inside `message`**, never at the top level of a turn. Turn 1 in full:

  ```json
  {"api": "anthropic.messages", "model": "claude-opus-5-5",
   "message": {"text": null, "reasoning": null,
               "tool_calls": [{"id": "toolu_capture_1", "name": "Bash",
                               "arguments": {"command": "echo hello", "description": "Print hello"}}]},
   "finish": "tool_calls",
   "usage": {"input": 420, "output": 12, "cache_read": 300, "cache_write": 20, "reasoning": 0}}
  ```

  A text turn is `"message": {"text": "…", "reasoning": null, "tool_calls": []}`. Every turn in C4, for all three agents, uses this shape: where this plan writes `text: …` or `tool_calls: […]` for a turn, it means inside `message`.

### Codex: `codex/exec-command`

**`main.ts`** (also `import path from "node:path";`, which only this scenario needs):
- `bin: "codex"`.
- `args: ["exec", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox", "Run echo hello"]`.
- `env`: `CODEX_HOME: path.join(home, ".codex")`, `MOCK_API_KEY: required("OPENAI_API_KEY")`.
- `homeFiles`: `".codex/config.toml"`, whose text `main.ts` builds with the endpoint from the environment:

  ```toml
  model = "gpt-5.5-codex"
  model_provider = "mock"

  [model_providers.mock]
  name = "Mock"
  base_url = "<CETRA_CAPTURE_MOCK_URL>/v1"
  env_key = "MOCK_API_KEY"
  wire_api = "responses"

  [otel]
  environment = "capture"
  exporter = "none"
  metrics_exporter = "none"
  trace_exporter = { otlp-http = { endpoint = "<CETRA_CAPTURE_OTLP_ENDPOINT>", protocol = "binary" } }

  [analytics]
  enabled = false
  ```

**`scenario.json`:**
- `description`: "Codex CLI 0.167.2 `codex exec` runs one exec_command tool call; trace export, filtered to the turn's spans."
- `spec_refs`: `["02 § Convention detection"]` (every scenario has it; it is required).
- `instrumentation` and `libraries`: `"@openai/codex"`.
- `span_names`: `["turn/start", "session_task.turn", "run_turn", "codex.sampling", "handle_responses", "exec_command"]`.
- `timing_attributes`: `["codex.response.receive_wait_ms", "busy_ns", "idle_ns", "thread.id", "dispatch_to_result_ready_ms"]`. `thread.id` is the OS thread number, an integer that `volatile_attributes` cannot mask (only strings are), and `dispatch_to_result_ready_ms` is a measured duration on an event.
- `volatile_attributes`: `["thread.id", "conversation.id", "turn.id", "turn_id", "thread_id", "submission.id", "unified_exec_process_id"]`.
- `turns`, two of them:
  1. `api: "openai.responses"`, `model: "gpt-5.5-codex"`, `tool_calls: [{"id": "call_capture_1", "name": "exec_command", "arguments": {"cmd": "echo hello"}}]`, `finish: "tool_calls"`, `usage: {input: 400, output: 30, cache_read: 300, cache_write: 0, reasoning: 10}`.
  2. `api: "openai.responses"`, `model: "gpt-5.5-codex"`, `text: "Done: the command printed hello."`, `finish: "stop"`, `usage: {input: 420, output: 25, cache_read: 300, cache_write: 0, reasoning: 5}`.

### Gemini CLI: `gemini_cli/shell-tool`

**`main.ts`:**
- `bin: "gemini"`.
- `args: ["-p", "Run echo hello with the shell", "--yolo"]`.
- `env`: `GEMINI_API_KEY: required("GEMINI_API_KEY")`, `GOOGLE_GEMINI_BASE_URL: required("CETRA_CAPTURE_MOCK_URL")`, `GEMINI_CLI_TRUST_WORKSPACE: "true"`.
- `homeFiles[".gemini/settings.json"]`: JSON. `otlpEndpoint` is `CETRA_CAPTURE_OTLP_ENDPOINT` with its trailing `/v1/traces` removed, because Gemini appends it:

  ```json
  {"telemetry": {"enabled": true, "traces": true, "target": "local",
                 "otlpEndpoint": "<base>", "otlpProtocol": "http", "logPrompts": true},
   "security": {"auth": {"selectedType": "gemini-api-key"}}}
  ```

  Gemini also posts `/v1/logs` and `/v1/metrics`. The receiver answers those two paths with 200 and discards them (`receiver.rs`); any other path would fail the capture.

**`scenario.json`:**
- `description`: "Gemini CLI 0.68.0 in -p mode: a model-routing call, then one run_shell_command tool call; traces with prompts."
- `spec_refs`: `["02 § Convention detection"]`.
- `instrumentation` and `libraries`: `"@google/gemini-cli"`.
- `timing_attributes`: `["gen_ai.input.messages", "gen_ai.system_instructions", "gen_ai.output.messages"]`. The first two hold today's date and the temporary paths; the output messages of `tool_call` and `schedule_tool_calls` hold measured durations and millisecond timestamps. The fixture keeps the values; only the recapture comparison skips them.
- `volatile_attributes`: `["gen_ai.conversation.id", "gen_ai.prompt.name", "gen_ai.tool.call_id", "session.id"]`.
- `turns`, three of them, all `api: "gemini.generate_content"`:
  1. `model: "gemini-3.5-flash-lite"`, `text: "{\"complexity_reasoning\": \"simple\", \"complexity_score\": 5}"`, `finish: "stop"`, `usage: {input: 400, output: 20, cache_read: 0, cache_write: 0, reasoning: 0}`. This is the routing call (non-streamed).
  2. `model: "gemini-3.8-flash"`, `tool_calls: [{"id": "gemini_capture_1", "name": "run_shell_command", "arguments": {"command": "echo hello", "description": "Print hello"}}]`, `finish: "tool_calls"`, `usage: {input: 900, output: 20, cache_read: 100, cache_write: 0, reasoning: 5}`.
  3. `model: "gemini-3.8-flash"`, `text: "Done: the command printed hello."`, `finish: "stop"`, `usage: {input: 950, output: 15, cache_read: 100, cache_write: 0, reasoning: 0}`.

**Steps:**
1. Write the three scenarios as above.
2. Run `just capture claude_code/bash-tool`. If the mock reports an unserved turn or an unknown route, the receiver reports a 415 (compression), the agent exits non-zero, or the case has no spans, stop and report the tool's error lines. Never copy prompts or outputs into the report.
3. Run the same for `codex/exec-command` and `gemini_cli/shell-tool`.
4. Check each `input.json`:
   - under 256 KiB (the tool enforces it);
   - no email address outside `@example.com` or `@example.org` (`cargo test -p cetra-otel --test fixtures` enforces it);
   - no occurrence of your real user name or host name. The scrubber's own `/home/capture` and `…/capture-agents/<case>/home/…` paths are expected.

   Report the span count per case.
5. Run `just check-capture` twice. The second run must show no difference. If one does, find the attribute that changes and stop and report it. The fix is a `volatile_attributes` or `timing_attributes` entry, which needs a plan change.
5b. **Resource attribute order.** Codex (Rust) emits its resource attributes in a different order on each run. In `run.rs` `json_differences`, before `diff`, sort every `resourceSpans[*].resource.attributes` array of both `committed_input` and `produced_input` by its `key` string, with a new `fn sort_resource_attributes(value: &mut Value)`. A stable sort keeps duplicate keys in their order. This changes only the check, not what is written. Test: `check_ignores_resource_attribute_order`, two inputs that differ only in that order give no differences.
6. `cargo test -p cetra-otel --test golden -- --nocapture` must pass. If `captured_llm_spans_are_priced_by_the_builtin_snapshot` fails for a new case, stop and report the span's model and provider. Do not change the snapshot or the scenario models. The models used (`claude-opus-5-5`, `gemini-3.5-flash-lite`, `gemini-3.8-flash`) are in `crates/otel/prices/builtin.json`, and Codex spans carry no model. Its output must contain `awaiting bless: claude_code/bash-tool`, `awaiting bless: codex/exec-command` and `awaiting bless: gemini_cli/shell-tool`.

**Done when:**
- Steps 2–6 pass.
- `cargo test -p cetra-otel --test fixtures` passes.
- `export PATH=…; just check` passes. `just check` does not run `check-capture`, so step 5 is separate.
- `git status` shows only this phase's files.

**Commit:** `Capture Claude Code, Codex and Gemini CLI fixtures`

---

## C5 — Hand-written Copilot case

**Goal:** a Copilot CLI turn as GitHub documents it, pinned with today's normalizer.

**Edit:** `fixtures/otlp/genai/copilot-cli-tool/{case.json,input.json,expected.json}` (new).

**Steps:**
1. `case.json`:

   ```json
   {
     "description": "GitHub Copilot CLI turn as documented by GitHub (spans invoke_agent, chat, execute_tool; GenAI semconv with content capture SPAN_ONLY).",
     "origin": "hand_written",
     "spec_refs": ["02 § Convention detection"],
     "notes": "Copilot has no base-URL override, so it cannot be captured (D-CA-12). Attributes from docs.github.com/en/copilot/reference/enterprise-administrators/opentelemetry-reference, 2026-11-20."
   }
   ```

2. `input.json` holds one OTLP/JSON request, following `fixtures/otlp/README.md` § input.json:
   - Resource: `service.name = "github-copilot"`.
   - Scope: `{name: "github.copilot", version: "1.0.0"}`.
   - Trace id `0af7651916cd43dd8448eb211c80319c`. Times fall inside the default window (§ case.json).
   - Spans:
     1. `invoke_agent` (root, span id `b7ad6b7169203331`, kind 1 INTERNAL, status code 1). Attributes:
        - `gen_ai.operation.name = "invoke_agent"`
        - `gen_ai.provider.name = "github"`
        - `gen_ai.agent.name = "copilot"`
        - `gen_ai.conversation.id = "conv-copilot-1"`
        - `enduser.pseudo.id = "pseudo-user-1"`
        - `gen_ai.request.model = "gpt-5.5"`
        - `gen_ai.usage.input_tokens = 900`
        - `gen_ai.usage.output_tokens = 40`
        - `gen_ai.usage.cache_read.input_tokens = 300`
        - `github.copilot.turn_count = 2`
        - `github.copilot.cost = 1`
        - `github.copilot.nano_aiu = 1200000`
     2. `chat gpt-5.5` (parent the root, span id `00f067aa0ba902b7`, kind 3 CLIENT). Attributes:
        - `gen_ai.operation.name = "chat"`
        - `gen_ai.provider.name = "github"`
        - `gen_ai.request.model = "gpt-5.5"`
        - `gen_ai.response.model = "gpt-5.5-2026-10-01"`
        - `gen_ai.usage.input_tokens = 900`
        - `gen_ai.usage.output_tokens = 40`
        - `gen_ai.usage.cache_read.input_tokens = 300`
        - `gen_ai.input.messages` = `[{"role":"user","parts":[{"type":"text","content":"Run echo hello"}]}]`
        - `gen_ai.output.messages` = `[{"role":"assistant","parts":[{"type":"tool_call","id":"call_1","name":"bash","arguments":{"command":"echo hello"}}],"finish_reason":"tool_call"}]`
        - `github.copilot.turn_id = "turn-1"`
     3. `execute_tool bash` (parent the root, span id `53995c3f42cd8ad8`, kind 1). Attributes:
        - `gen_ai.operation.name = "execute_tool"`
        - `gen_ai.tool.name = "bash"`
        - `gen_ai.tool.call.id = "call_1"`
        - `gen_ai.tool.call.arguments = "{\"command\":\"echo hello\"}"`
        - `gen_ai.tool.call.result = "hello"`
   - Times: every span in `[1767225595000000000, 1767225598000000000]` ns (2025-12-31T23:59:55Z–58Z), children inside their parent. Status codes: 1 on every span. Then run `just fixtures-fmt`.
3. Write `expected.json` with `just fixtures-bless`. **It blesses every case lacking `expected.json`**, including C4's three captured cases. Delete those three files right after (`rm fixtures/otlp/{claude_code/bash-tool,codex/exec-command,gemini_cli/shell-tool}/expected.json`); M2 blesses them. Do not change `fixtures/otlp/prices.json`: it has no `gpt-5.5` row, so the `chat` span's cost is `none`. Then review it against spec 02 and check it is what today's normalizer must give:
   - the root is `agent` with null tokens (rule 3);
   - `chat` is `llm` with the tokens, and cost from `fixtures/otlp/prices.json` when it prices `gpt-5.5`, else `none`;
   - the tool is `tool`, with `input` from `gen_ai.tool.call.arguments` and `output` from `gen_ai.tool.call.result`;
   - `session_id = "conv-copilot-1"`;
   - `user_id` is null, because `enduser.pseudo.id` is not a source yet (M2 adds it).

   If the bless writes anything else, stop and report.

**Done when:**
- `just fixtures-check` passes.
- `cargo test -p cetra-otel --test golden` and `--test fixtures` pass.
- `git status --porcelain fixtures/otlp` lists only the three files of `genai/copilot-cli-tool`.

**Commit:** `Add a hand-written Copilot CLI fixture`

---

## After M1

Not in this plan, and why:
- **The design's `ENV_PREFIXES` change.** It is not needed: agent scenarios keep `env` empty and set the child's environment in `main.ts`.
- **Identity scrubbing.** It is covered by the isolated home, the fake git identity, the scrubber's existing resource and path rules, and the fixtures secret scan.
- **The hook session-id checks.** They were done in the spike (research § 6): verified for Claude Code and Gemini CLI, impossible for Codex until its hooks are trusted.

`just check-capture` now also runs three agent CLIs, each a few seconds; CI's `check-capture` job gets that much slower.


The coordinator (not Haiku) reviews the captured inputs against research § 6, updates the research doc if they differ, and opens the PR stacked on the design branch.
