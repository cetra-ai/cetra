Status: plan, written 2026-11-20 from the code at `ad50b2d`.

# CA · M4 — `cetra agents hook | setup | remove`

Milestone M4 of track CA:
- design `docs/plans/ca-coding-agents/00-design.md` § Session context › Hook, § Setup command;
- decisions D-CA-8 and D-CA-10;
- the agents' behaviour, verified by the spike: `docs/research/coding-agents.md` § 6.

**Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

## Prerequisites

- Branch `ca-m4-cli`, cut from `claude/ca-coding-agents-22e274`. It does not need M1–M3: the hook posts to `POST /v1/agent-context` (M3), and the tests use a stub server.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just`.
- Load rule: compile and test Rust once per phase, right before the commit.

## Read first

- `AGENTS.md` (errors: `anyhow` is allowed in `cetra-cli`; privacy; no `unwrap` outside tests).
- `crates/cli/src/main.rs`:
  - `Command`, `run_cli`;
  - the `Projects` dispatch (lines 266–325) as the model;
  - `cli_reference` and `cli_reference_is_up_to_date` (lines 920–968).
- `crates/cli/src/credentials.rs`: `credentials_path`, `save`, `write_private`, `set_dir_mode`.
- `crates/cli/src/client.rs`:
  - `Token`, `ConnectionArgs` (the `hide_env_values` pattern);
  - `ApiClient::new` (reqwest builder settings).
- `crates/cli/src/test_server.rs`: `Canned` (stub HTTP server, `recorded()`).
- `crates/cli/src/login.rs`: `browser_command`, the one existing `std::process::Command` use.

## What it is, and what it is not

**When M4 is done:**
- `cetra agents setup <claude-code|codex|gemini-cli|copilot> --url <server>`, with the key in `CETRA_INGEST_KEY` or `--key`, configures the agent:
  - it writes `agents.toml` and the agent's user-level settings;
  - for Copilot, it prints the settings instead.
- `cetra agents remove <agent>` removes what setup wrote.
- `cetra agents hook <agent>` (hidden), run by the agent with its hook JSON on stdin, posts the session context and always exits 0.

**Not in M4:**
- The server route (M3).
- The docs pages (M5b).
- Windows: the hook and setup are built for unix paths, and setup refuses on Windows with "cetra agents is not supported on Windows".

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-8 | The hook: git calls with timeouts, user-info stripped, `null` rules, never stdout, always exit 0, 2 s in all |
| D-CA-9 | The route body `{agent, session_id, repository, branch, user}` |
| D-CA-10 | Setup writes user-level settings; content on by default; logs and metrics off; idempotent; one backup; absolute exe path in hooks |
| D-P-70 | A PAT cannot create keys, so setup takes an existing `sk` key |

Spike facts used here (research § 6):
- **Claude Code.** Hooks in `~/.claude/settings.json` have the shape `{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command": "…"}]}]}}`.
- **Gemini CLI.** Hooks use the same shape in `~/.gemini/settings.json`. Gemini **ignores `telemetry.otlpHeaders`** (no header was sent), but it reads `OTEL_EXPORTER_OTLP_HEADERS` from `~/.gemini/.env`.
- **Codex.** Hooks are read from `$CODEX_HOME/hooks.json` in the same shape; `config.toml` hook tables did not run. A hook runs only after the user trusts it in Codex.
  - *Unverified:* whether Codex parses our `hooks.json` before it is trusted. Setup writes it, and the docs (M5b) tell the user to approve it.

## Versions

| Crate | Version | Where | Why |
|---|---|---|---|
| `toml_edit` | `0.25.15` (already in `Cargo.lock` as `0.25.15+spec-1.1.0`; the registry's latest is `0.25.19+spec-1.1.0`) | root `[workspace.dependencies]`: `toml_edit = { version = "0.25.15", default-features = false, features = ["parse", "display"] }`; `crates/cli/Cargo.toml`: `toml_edit = { workspace = true }` | to edit `~/.codex/config.toml` in place, keeping comments and order |

Re-check with `cargo info toml_edit`. Keep `0.25.15` unless it no longer resolves, and report the version used.

## Phases

```
A1 (hook) ──► A2 (setup/remove: claude-code + copilot) ──► A3 (codex + gemini-cli) ──► A4 (docs, reference)
```

All four edit `crates/cli/src/agents/` and run in order.

---

## A1 — `cetra agents hook <agent>`

**Goal:** the hidden hook command, `agents.toml`, and the `agents` command group.

**Edit:**
- `crates/cli/src/main.rs`
- `crates/cli/src/credentials.rs`
- `crates/cli/src/agents/mod.rs` (new)
- `crates/cli/src/agents/hook.rs` (new)

**Steps:**
1. **`credentials.rs`.** Extract the body of `save` that creates the directory, writes `<name>.tmp` privately and renames it, into:

   ```rust
   /// Writes `bytes` to `path` with mode 0600 (unix), through `<path>.tmp` and a rename.
   /// Creates the parent directory with mode 0700 (unix).
   pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()>
   ```

   `save` calls it, so its behaviour is unchanged. Its existing tests must pass unchanged.
2. **`agents/mod.rs`.** Define:

   ```rust
   /// The coding agents `cetra agents` configures (D-CA-1). Wire names are D-CA-4's.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
   pub(crate) enum Agent {
       #[value(name = "claude-code")] ClaudeCode,
       #[value(name = "codex")] Codex,
       #[value(name = "gemini-cli")] GeminiCli,
       #[value(name = "copilot")] Copilot,
   }
   impl Agent {
       pub(crate) fn wire_name(self) -> &'static str; // "claude-code" | "codex" | "gemini-cli" | "copilot"
   }

   /// `<config dir>/cetra/agents.toml`: what the hook needs.
   #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
   pub(crate) struct AgentsFile {
       pub(crate) url: String,
       pub(crate) key: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub(crate) user: Option<String>,
   }

   /// Where `cetra agents` reads and writes. Production values come from `Paths::detect`; tests pass temporary directories.
   #[derive(Debug, Clone)]
   pub(crate) struct Paths {
       pub(crate) home: PathBuf,       // dirs::home_dir()
       pub(crate) cetra_dir: PathBuf,  // dirs::config_dir()/cetra (same directory as credentials.toml)
       pub(crate) exe: PathBuf,        // std::env::current_exe()
   }
   impl Paths {
       pub(crate) fn detect() -> anyhow::Result<Self>;
       pub(crate) fn agents_file(&self) -> PathBuf { self.cetra_dir.join("agents.toml") }
   }

   #[derive(Debug, clap::Subcommand)]
   pub(crate) enum AgentsCommand {
       /// Run by an agent's hook: send the session's repository, branch and person to cetra.
       #[command(hide = true)]
       Hook { agent: Agent },
   }

   pub(crate) async fn run(command: &AgentsCommand, paths: &Paths) -> anyhow::Result<()>;
   ```

   `AgentsFile::load(path) -> anyhow::Result<Option<AgentsFile>>` returns `None` when the file is missing, and the error "agents.toml is invalid" on a parse error, never echoing the content. `save(path)` serializes with `toml::to_string` and calls `write_private_file`.
3. **`main.rs`.**
   - Add `mod agents;`.
   - Add the `Command` variant `/// Connect coding agents (Claude Code, Codex, Gemini CLI, Copilot) to cetra.` `Agents { #[command(subcommand)] command: Box<agents::AgentsCommand> }`.
   - Add the dispatch arm `Command::Agents { command } => agents::run(&command, &agents::Paths::detect()?).await?`.
   - **Exception for the hook.** It must never fail. If `Paths::detect()` fails, the hook arm writes `cetra agents hook: <error>` to stderr and returns `Ok(())`. To achieve this, match `AgentsCommand::Hook` before calling `detect`.
4. **`agents/hook.rs`.** The flow of `pub(crate) async fn hook(agent: Agent, paths: &Paths, stdin: impl Read + Send + 'static) -> ()`, which never returns an error:
   1. **Deadline.** `let deadline = Instant::now() + Duration::from_secs(2);`. Everything below respects it.
   2. **Read stdin.** On a `std::thread`, read stdin to its end, at most 64 KiB (`take(65_536)`), and send the bytes through a `std::sync::mpsc` channel. Wait with `recv_timeout(Duration::from_millis(500))`. On a timeout, note "no hook input" and return.
   3. **Parse** the JSON into `struct HookInput { session_id: String, cwd: String }`. Ignore unknown fields (`#[serde(default)]` is not needed, as both fields are required). On failure, note "hook input is not valid JSON" and return. A `session_id` empty or longer than 256 characters: note "hook input has no session id" and return.
   4. **Load `AgentsFile`.** On `None`, note "not set up; run `cetra agents setup`" and return. On an error, note it and return.
   5. **Repository.** `git_output(&["-C", &cwd, "remote", "get-url", "origin"], 1 s)`, then `strip_user_info` on the trimmed output; empty output is `None`.
   6. **Branch.** `git_output(&["-C", &cwd, "rev-parse", "--abbrev-ref", "HEAD"], 1 s)`, trimmed. The value `"HEAD"` (detached) or empty is `None`.
   7. **Post.** `POST {url}/v1/agent-context` with `Authorization: Bearer {key}`, `Content-Type: application/json`, and the body below. `repository` and `branch` are always present (`null` when `None`), and `user` is present only when `AgentsFile.user` is set:

      ```json
      {"agent": "<wire name>", "session_id": "…", "repository": "…"|null, "branch": "…"|null, "user": "…"}
      ```

      - Use a `reqwest::Client` with `connect_timeout` and `timeout` equal to the time left before the deadline (minimum 100 ms), and `redirect(Policy::none())`.
      - A non-2xx status: note `server answered <status code>`.
      - A transport error: note "could not reach the server".
   8. **Notes.** Each note is one line on **stderr**: `cetra agents hook: <note>`. It never contains a URL, key, path, branch, email or the stdin content. Nothing is ever written to stdout.
5. In `hook.rs`:

   ```rust
   /// Runs `git <args>` with no stdin, captures stdout, and kills it at `timeout`.
   /// `None` on spawn failure, timeout, non-zero exit or non-UTF-8 output.
   fn git_output(args: &[&str], timeout: Duration) -> Option<String>
   ```

   - It spawns `std::process::Command::new("git")` with stdout piped, and stdin and stderr null.
   - It polls `try_wait` every 10 ms until `timeout`, then calls `kill` and `wait`.
   - It reads stdout only after exit (the output is small).
   - For testability, the program name is a parameter of an inner `fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String>`, and `git_output` calls it with `"git"`.

   ```rust
   /// Removes `user[:password]@` from a `scheme://` URL. Any other text (scp-like `git@host:o/r.git`, paths) is returned unchanged.
   fn strip_user_info(remote: &str) -> String
   ```

   It finds `://`, then the first `/` after it (or the end of the string). If an `@` lies between them, it removes everything from just after `://` through that `@`. If there are several `@`, it removes through the last one before that `/`.

**Tests** (`hook.rs` `mod tests`, and `credentials.rs`):
- `strip_user_info`:
  - `https://tok@github.com/o/r.git` → `https://github.com/o/r.git`;
  - `https://u:p@ss@host/o/r` → `https://host/o/r`;
  - `git@github.com:o/r.git` unchanged;
  - `https://github.com/o/r` unchanged;
  - `/local/path` unchanged.
- `run_with_timeout`:
  - `("sleep", &["5"], 100 ms)` returns `None` within 1 s;
  - `("echo", &["hi"], 1 s)` returns `Some("hi\n")`;
  - a missing program returns `None`.
- `hook_posts_repository_and_branch`:
  - Setup: a temp git repo (`git init -b feature/x`, remote `https://tok@github.com/Acme/Widgets.git`, one empty commit, with `GIT_CONFIG_NOSYSTEM=1` and `HOME` set for the git calls), a `Canned::json(vec![(204, String::new())])` server, and `agents.toml` with its URL, key `sk-cetra-test` and user `dev@example.com`.
  - Run `hook(Agent::ClaudeCode, …)` with stdin `{"session_id":"s1","cwd":"<repo>","hook_event_name":"SessionStart"}`.
  - The recorded request is `POST /v1/agent-context` with `authorization: Bearer sk-cetra-test`, and the body equals `{"agent":"claude-code","session_id":"s1","repository":"https://github.com/Acme/Widgets.git","branch":"feature/x","user":"dev@example.com"}`.
- `hook_sends_null_outside_git`: `cwd` is a plain temp dir; the body has `"repository":null,"branch":null`, and no `user` key when `AgentsFile.user` is `None`.
- `hook_sends_null_branch_when_detached`: detach HEAD (`git checkout --detach`); `branch` is `null`.
- `hook_without_agents_toml_posts_nothing`.
- `hook_with_bad_input_posts_nothing`: invalid JSON, and JSON without `session_id`.
- `hook_never_fails_when_the_server_is_down`: `url` points to a closed port. The function returns within 3 s.
- `hook_command_exits_zero_and_prints_nothing`: through `run_cli(["cetra", "agents", "hook", "codex"])` with no `agents.toml`. It must not panic, and it returns `Ok`. Use a seam: `run` takes `&Paths`, so test `agents::run` with temp `Paths`.
- `agents_file_round_trips_with_mode_0600` (unix).
- `credentials.rs`: the existing tests still pass.

**Done when:**
- `cargo test -p cetra-cli agents` passes.
- `cargo test -p cetra-cli credentials` passes.
- `cargo clippy -p cetra-cli --all-targets -- -D warnings` passes.

**Commit:** `Add the cetra agents hook command`

---

## A2 — `setup` and `remove` for Claude Code, and Copilot's printout

**Goal:** `cetra agents setup claude-code` and `remove claude-code` edit `~/.claude/settings.json`, and `setup copilot` prints its settings.

**Edit:**
- `crates/cli/src/agents/mod.rs`
- `crates/cli/src/agents/setup.rs` (new)

**Steps:**
1. **The CLI surface.** Add to `AgentsCommand`:

   ```rust
   /// Configure an agent to send its traces to cetra (writes the agent's user settings).
   Setup(SetupArgs),
   /// Remove what `cetra agents setup` wrote for an agent.
   Remove { agent: Agent },

   #[derive(Debug, clap::Args)]
   pub(crate) struct SetupArgs {
       pub(crate) agent: Agent,
       /// cetra server URL, for example `https://cetra.example.com`.
       #[arg(long)]
       pub(crate) url: String,
       /// Project secret key with the `ingest` scope. A value given as a flag is visible to other local users in the process list; prefer CETRA_INGEST_KEY.
       #[arg(long, env = "CETRA_INGEST_KEY", hide_env_values = true)]
       pub(crate) key: String,
       /// The person to attribute sessions to (default: `git config --global user.email`).
       #[arg(long)]
       pub(crate) user: Option<String>,
       /// Do not turn on prompt and tool content capture.
       #[arg(long)]
       pub(crate) no_content: bool,
       /// Print the settings instead of writing them.
       #[arg(long)]
       pub(crate) print: bool,
   }
   ```
2. **`setup`** (`pub(crate) fn setup(args: &SetupArgs, paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()>`), in order:
   1. On `cfg!(windows)`: error "cetra agents is not supported on Windows".
   2. Check the URL with `credentials::check_server_url` and normalize it with `credentials::server_url`. Read both functions first and reuse them as they are. If their signatures do not fit, stop and report.
   3. `key` must start with `sk-cetra-`, else the error "the key must be a project secret key (sk-cetra-…) with the ingest scope". The error never echoes the key.
   4. `user`: `--user`, else `run_with_timeout("git", &["config", "--global", "user.email"], 1 s)` trimmed. Lowercase it and trim it. Empty becomes `None`. Make `run_with_timeout` `pub(super)` in `hook.rs`.
   5. Unless `--print`, save `AgentsFile { url, key, user }` to `paths.agents_file()`.
   6. Dispatch on the agent: `claude_code`, `copilot` (this phase), `codex`, `gemini_cli` (A3). Until A3, the codex and gemini-cli arms must not exist; A3 adds them. `Agent::Codex | Agent::GeminiCli` in A2 return the error "not implemented yet".
   7. Each agent function writes the result lines to `out`: `wrote <path>` for each file written, then the notes of § Notes.
3. **Shared JSON helpers in `setup.rs`:**

   ```rust
   const HOOK_SUFFIX: &str = " agents hook "; // a command is ours when it contains this followed by the agent's wire name at the end
   fn hook_command(paths: &Paths, agent: Agent) -> String; // "<quoted exe> agents hook <wire name>"
   fn shell_quote(path: &Path) -> String;
   fn is_our_hook(command: &str, agent: Agent) -> bool; // command.ends_with(&format!("agents hook {}", agent.wire_name()))
   fn set_hooks(root: &mut serde_json::Map<String, Value>, events: &[&str], command: &str, agent: Agent);
   fn remove_hooks(root: &mut serde_json::Map<String, Value>, agent: Agent);
   fn read_json_object(path: &Path) -> anyhow::Result<serde_json::Map<String, Value>>; // missing file → empty map; invalid → error "<path> is not valid JSON; fix it or run with --print"
   fn write_json_object(path: &Path, value: &serde_json::Map<String, Value>) -> anyhow::Result<()>; // backup, then pretty JSON (2 spaces) + "\n" via write_private_file
   fn backup_once(path: &Path) -> anyhow::Result<()>; // if path exists and "<path>.cetra-backup" does not, copy it
   ```

   - **`shell_quote`.** It returns the path as is when every character is in `[A-Za-z0-9/._+:@%=,-]`. Otherwise it wraps the path in single quotes and writes each `'` as `'\''`.
   - **`set_hooks`.** For each event:
     1. Take `root["hooks"][event]`, created as an empty array if absent.
     2. Remove every element whose `hooks` array contains a command for which `is_our_hook` holds; elements emptied that way go.
     3. Append `{"hooks": [{"type": "command", "command": <command>, "timeout": 5}]}`.
     4. Other elements and other events stay as they were, in their order.
   - **`remove_hooks`.** It removes our elements from every event, drops events whose array is left empty, and drops `"hooks"` if it is left empty.
   - **`serde_json` key order.** The workspace enables `preserve_order`, so existing key order is kept.
4. **Claude Code** (`fn claude_code(…)`), file `<home>/.claude/settings.json`.
   - **Setup.**
     - `env` is an object, created if absent. Set:
       - `CLAUDE_CODE_ENABLE_TELEMETRY=1`
       - `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`
       - `OTEL_TRACES_EXPORTER=otlp`
       - `OTEL_LOGS_EXPORTER=none`
       - `OTEL_METRICS_EXPORTER=none`
       - `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`
       - `OTEL_EXPORTER_OTLP_ENDPOINT=<url>`
       - `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`
       - `OTEL_METRICS_INCLUDE_REPOSITORY=true`
     - With content, also set `OTEL_LOG_USER_PROMPTS=1`, `OTEL_LOG_TOOL_DETAILS=1` and `OTEL_LOG_TOOL_CONTENT=1`. With `--no-content`, remove those three keys.
     - All values are JSON strings.
     - `set_hooks(root, &["SessionStart", "UserPromptSubmit", "CwdChanged"], …)`.
   - **Remove.** Read `agents.toml` for its `url`. If `env.OTEL_EXPORTER_OTLP_ENDPOINT` equals that URL, remove all twelve env keys above, and drop `env` if it is left empty. Otherwise leave `env` untouched and note "the telemetry settings point to another server; left unchanged". In both cases call `remove_hooks`. Write the file only if it changed.
   - **`--print`.** Print the merged JSON, exactly as it would be written. Write nothing and save no `agents.toml`.
5. **Copilot** (`fn copilot(…)`). It writes nothing, with or without `--print`. It prints:

   ```
   # Copilot CLI: add to your shell profile (Copilot CLI only exports to https:// endpoints)
   export COPILOT_OTEL_ENABLED=true
   export OTEL_EXPORTER_OTLP_ENDPOINT=<url>
   export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
   export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer%20<key>'
   export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY      # omitted with --no-content

   # VS Code: add to settings.json; the key comes from OTEL_EXPORTER_OTLP_HEADERS in VS Code's environment
   {
     "github.copilot.chat.otel.enabled": true,
     "github.copilot.chat.otel.exporterType": "otlp-http",
     "github.copilot.chat.otel.otlpEndpoint": "<url>",
     "github.copilot.chat.otel.captureContent": true
   }
   ```

   With `--no-content`, `captureContent` is `false`. `remove copilot` prints "nothing to remove: cetra agents setup copilot writes no file".
6. **Notes.** These are printed after the `wrote` lines.
   - Claude Code: "Start a new Claude Code session to apply the settings."
   - When the URL starts with `http://` and the agent is Copilot: "Copilot CLI does not export to http:// endpoints; put TLS in front of cetra."

**Tests** (`setup.rs`, on a temporary `Paths` with `exe = /opt/cetra/bin/cetra`):
- `shell_quote` keeps `/opt/cetra/bin/cetra`, and quotes `/Users/a b/cetra` and `/tmp/it's/cetra`.
- `claude_setup_on_empty_home_writes_settings`: the file has exactly the env keys and the three hook events. `agents.toml` is saved with mode 0600.
- `claude_setup_keeps_other_settings`: an existing file with `{"model": "x", "env": {"FOO": "1"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other"}]}]}}`. After setup, `model`, `FOO` and the `Stop` hook are still there, in their places. `settings.json.cetra-backup` equals the original bytes.
- `claude_setup_twice_is_idempotent`: the second run gives identical file bytes, one hook entry per event, and the backup still equals the original.
- `claude_setup_no_content_removes_content_flags`.
- `claude_setup_rejects_invalid_json_and_writes_nothing`.
- `claude_print_writes_nothing`.
- `claude_remove_restores_the_other_settings`: setup on the file of `claude_setup_keeps_other_settings`, then remove. The JSON equals the original's value: compare `serde_json::Value`, not bytes.
- `claude_remove_leaves_env_pointing_elsewhere`.
- `setup_refuses_a_non_sk_key` (the message does not contain the key) and `setup_refuses_a_bad_url`.
- `copilot_prints_and_writes_nothing`, `copilot_no_content`.

**Done when:**
- `cargo test -p cetra-cli agents` passes.
- `cargo clippy -p cetra-cli --all-targets -- -D warnings` passes.

**Commit:** `Add cetra agents setup and remove for Claude Code and Copilot`

---

## A3 — Codex and Gemini CLI

**Goal:** setup and remove for Codex (`config.toml` + `hooks.json`) and Gemini CLI (`settings.json` + `.env`).

**Edit:**
- `Cargo.toml` (root: `toml_edit`)
- `crates/cli/Cargo.toml`
- `Cargo.lock`
- `crates/cli/src/agents/setup.rs`
- `crates/cli/src/agents/codex.rs` (new)

**Steps:**
1. Add `toml_edit` as in § Versions.
2. **Codex** (`codex.rs`). The Codex home is `$CODEX_HOME` when set and non-empty, else `<home>/.codex`. The function takes it as a parameter; `setup` reads the environment variable.
   - **Setup, `config.toml`.** Parse it with `toml_edit::DocumentMut`; a missing file is an empty document, and an invalid one is the error "<path> is not valid TOML; fix it or run with --print". Ensure `[otel]` is a table, then set:
     - `exporter = "none"`
     - `metrics_exporter = "none"`
     - `trace_exporter = { otlp-http = { endpoint = "<url>/v1/traces", protocol = "binary", headers = { authorization = "Bearer <key>" } } }`, as an inline table (`toml_edit::InlineTable`)

     Leave every other key and comment as it was. Back up once, then write it with `write_private_file`.
   - **Setup, `hooks.json`.** In `<codex home>/hooks.json`, apply `set_hooks(&["SessionStart", "UserPromptSubmit"], …)` with the A2 helpers. Back up once, then write.
   - **`--no-content`** changes nothing for Codex: its spans carry no content.
   - **Remove.** If `otel.trace_exporter.otlp-http.endpoint` equals `<agents.toml url>/v1/traces`, remove `trace_exporter`, then `exporter` and `metrics_exporter` when their value is `"none"`, and drop `[otel]` if it is left empty. Otherwise note "the telemetry settings point to another server; left unchanged". Remove our hooks from `hooks.json`. Write only what changed.
   - **Note.** "Codex runs the cetra hook only after you trust it in Codex; until then, traces have no repository or branch."
   - **`--print`** prints both files' new contents, each under a `# <path>` line.
3. **Gemini CLI** (in `setup.rs`).
   - **Setup, `settings.json`.** In `<home>/.gemini/settings.json`, set the `telemetry` object: `enabled: true`, `traces: true`, `target: "local"`, `otlpEndpoint: "<url>"`, `otlpProtocol: "http"`, and `logPrompts: true` (`false` with `--no-content`). Keep the other `telemetry` keys. Then `set_hooks(&["SessionStart", "BeforeAgent"], …)`.
   - **Setup, `.env`.** In `<home>/.gemini/.env`, read the lines (a missing file is empty) and replace the line starting `OTEL_EXPORTER_OTLP_HEADERS=` with `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`; append the line if absent. Keep the other lines and their order, with a trailing newline. Back up once, then write with `write_private_file`. The spike showed that Gemini ignores `telemetry.otlpHeaders` and reads this file.
   - **Remove.** If `telemetry.otlpEndpoint` equals the `agents.toml` URL, remove the six keys set above, and drop `telemetry` if it is left empty. Remove the `.env` line `OTEL_EXPORTER_OTLP_HEADERS=` if its value is `Authorization=Bearer%20<agents.toml key>`. Remove our hooks.
   - **Note.** "Gemini CLI also sends logs and metrics; cetra accepts and drops them."
4. Replace A2's "not implemented yet" arms with the real calls.

**Tests:**
- `codex_setup_keeps_comments_and_other_keys`: an existing `config.toml` with a comment line, `model = "x"` and a `[profiles.a]` table. After setup they are unchanged in the text, and `[otel]` has the three keys.
- `codex_setup_twice_is_idempotent` (byte-identical).
- `codex_remove_restores_the_document`: setup then remove gives the original text, except for a trailing-newline difference. Check whether `toml_edit` round-trips exactly. If it does not, assert key equality instead and say so in the report.
- `codex_hooks_json_has_two_events`.
- `codex_home_from_env`: the function, given another directory, writes there.
- `gemini_setup_writes_settings_and_env`: the `.env` keeps `GEMINI_API_KEY=x`, which was there before.
- `gemini_setup_twice_is_idempotent`.
- `gemini_remove_restores`.
- `gemini_no_content_sets_log_prompts_false`.

**Done when:**
- `cargo test -p cetra-cli agents` passes.
- `cargo clippy -p cetra-cli --all-targets -- -D warnings` passes.
- `cargo deny`-style family checks are not needed: `toml_edit` is already in the lock.

**Commit:** `Add cetra agents setup and remove for Codex and Gemini CLI`

---

## A4 — Docs: spec 09, the CLI reference, `CETRA_INGEST_KEY`

**Goal:** the generated CLI reference and spec 09 list `cetra agents`.

**Edit:**
- `crates/cli/src/main.rs` (the intro sentence of `cli_reference` only)
- `website/src/content/docs/docs/reference/cli.md` (generated)
- `docs/spec/09-api.md`

**Steps:**
1. In `cli_reference` (`main.rs` lines 931–933), the sentence listing the remote commands adds `` `cetra agents` ``.
2. Run `CETRA_DOCS_WRITE=1 cargo test -p cetra-cli cli_reference_is_up_to_date`. The page gains `agents setup` and `agents remove`, with `[env: CETRA_INGEST_KEY]`. `agents hook` is hidden and must not appear; check it.
3. In spec 09 § CLI, the table gains these rows after `projects list`:
   - `` | `cetra agents setup <agent> --url` | configure Claude Code, Codex, Gemini CLI or Copilot to send traces to cetra; key from `CETRA_INGEST_KEY` or `--key`; `--user`, `--no-content`, `--print` (D-CA-10) | ``
   - `` | `cetra agents remove <agent>` | remove what setup wrote (D-CA-10) | ``

   Add the sentence: "`cetra agents hook <agent>` is hidden: the agents' hooks run it to send the session context (D-CA-8)."
4. `CETRA_INGEST_KEY` is now named in `crates/cli/src` (the clap `env`), so the docs check `check-env-vars` accepts it in the generated page.

**Done when:**
- `cargo test -p cetra-cli` passes.
- `export PATH=…; just check` passes.

**Commit:** `Document cetra agents in the CLI reference and spec 09`
