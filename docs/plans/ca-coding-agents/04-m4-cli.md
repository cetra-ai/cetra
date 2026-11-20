Status: plan, written 2026-11-20 from the code at `ad50b2d`. Cold-reviewed once; the findings are settled here.

# CA · M4 — `cetra agents hook | setup | remove`

Milestone M4 of track CA:
- design `docs/plans/ca-coding-agents/00-design.md` § Session context › Hook, § Setup command;
- decisions D-CA-8 and D-CA-10;
- agent behaviour verified by the spike: `docs/research/coding-agents.md` § 6.

**Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

## Prerequisites

- Branch `ca-m4-cli`, cut from `claude/ca-coding-agents-22e274`. It does not need M1–M3: the hook posts to `POST /v1/agent-context` (M3), and the tests use a stub server.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just`.
- Load rule: compile and test Rust once per phase, right before the commit.

## Read first

- `AGENTS.md` (`anyhow` is allowed in `cetra-cli`; privacy; no `unwrap` outside tests; no `unsafe`, so never `std::env::set_var`).
- Workspace lints in the root `Cargo.toml`: `print_stdout` and `print_stderr` are denied by clippy `-D warnings`. Write with `writeln!` to a writer, as `main.rs` does.
- `crates/cli/src/main.rs`:
  - `Command`, `run_cli`, the `Projects` dispatch with `output::Io`;
  - `cli_reference` and `cli_reference_is_up_to_date`.
- `crates/cli/src/credentials.rs`: `credentials_path`, `save`, `write_private`, `set_dir_mode`, `server_url`.
- `crates/cli/src/client.rs`: `Token` (its redacting `Debug` and `FromStr`), `ConnectionArgs` (`env`, `hide_env_values`).
- `crates/cli/src/test_server.rs`: `Canned::json` and `recorded()`. `Recorded` has `target`, `head` (request line plus lowercase headers) and `body`.

## What it is, and what it is not

**When M4 is done:**
- `cetra agents setup <claude-code|codex|gemini-cli|copilot> --url <server>` configures the agent. The key comes from `CETRA_INGEST_KEY` or `--key`. Setup writes `agents.toml` and the agent's user-level settings; for Copilot it prints the settings instead.
- `cetra agents remove <agent>` removes what setup wrote.
- `cetra agents hook <agent>` (hidden), run by the agent with its hook JSON on stdin, posts the session context and always exits 0.

**Not in M4:**
- The server route (M3).
- Docs pages (M5b).
- Windows: `setup` and `remove` refuse with "cetra agents is not supported on Windows", and the hook does nothing there.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-8 | The hook: git calls with timeouts, user-info stripped, `null` rules, never stdout, always exit 0, 2 s in all |
| D-CA-9 | The route body `{agent, session_id, repository, branch, user}` |
| D-CA-10 | Setup writes user-level settings (Gemini's key in `~/.gemini/.env`); content on by default; logs and metrics off; idempotent; one backup; absolute exe path in hooks |
| D-P-70 | A PAT cannot create keys, so setup takes an existing `sk` key |

**Spike facts used here** (research § 6):
- **Hook shape.** Claude Code (`~/.claude/settings.json`), Gemini CLI (`~/.gemini/settings.json`) and Codex (`$CODEX_HOME/hooks.json`) all use `{"hooks": {"<Event>": [{"hooks": [{"type": "command", "command": "…"}]}]}}`.
- **Hook stdin.** All three close stdin after writing the JSON: the spike's hooks read it with `cat`.
- **Claude Code** sends `vcs.*` on spans with `OTEL_METRICS_INCLUDE_REPOSITORY=true`.
- **Gemini CLI** ignores `telemetry.otlpHeaders`, and reads `OTEL_EXPORTER_OTLP_HEADERS` from `~/.gemini/.env`.
- **Codex** runs hooks only after the user trusts them. *Unverified:* whether it parses our `hooks.json` before then.

**Rules this plan settles** (details that are not design decisions):
- **`agents.toml` holds one server.** Each `setup` (except Copilot's) rewrites it with that setup's URL, key and user. `remove` never deletes it.
- **When `remove` cannot find our endpoint,** it changes nothing in the telemetry keys and says so; our hooks are still removed.
- **`--print` prints the key,** since the printed settings must work. Its help says so.
- **A symlinked settings file** is resolved with `fs::canonicalize`; the target is written and the link is kept.

## Versions

| Crate | Version | Where | Why |
|---|---|---|---|
| `toml_edit` | `0.25.15` (already in `Cargo.lock` as `0.25.15+spec-1.1.0`; registry latest `0.25.19+spec-1.1.0`) | root `[workspace.dependencies]`: `toml_edit = { version = "0.25.15", default-features = false, features = ["parse", "display"] }`; `crates/cli/Cargo.toml`: `toml_edit = { workspace = true }` | edit `~/.codex/config.toml` in place, keeping comments and order |

Re-check with `cargo info toml_edit`. If those features do not exist under those names, stop and report. Report the version used.

## Phases

```
A1 (hook, agents.toml, command group) ──► A2 (setup and remove, four agents) ──► A3 (docs, reference, changelog)
```

---

## A1 — `cetra agents hook <agent>`

**Goal:** the hidden hook command, `agents.toml`, and the `agents` command group.

**Edit:**
- `crates/cli/src/main.rs`
- `crates/cli/src/credentials.rs`
- `crates/cli/src/agents/mod.rs` (new)
- `crates/cli/src/agents/hook.rs` (new)

**Steps:**
1. **`credentials.rs`.** Add:

   ```rust
   /// Writes `bytes` to `path` with mode 0600 (unix), through `<path>.tmp` and a rename.
   /// Creates missing parent directories, but never changes the mode of an existing directory.
   pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()>
   ```

   It does `create_dir_all(parent)`, then `write_private(<path>.tmp, bytes)`, then `fs::rename`. `save` keeps its own `set_dir_mode` call and uses `write_private_file` for the rest, so its behaviour is unchanged.
2. **`agents/mod.rs`:**

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

   /// `<config dir>/cetra/agents.toml`: what the hook needs. Debug hides the key.
   #[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
   pub(crate) struct AgentsFile {
       pub(crate) url: String,
       pub(crate) key: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub(crate) user: Option<String>,
   }
   // impl fmt::Debug for AgentsFile: `AgentsFile { url, key: "***", user: Some("***") | None }`
   impl AgentsFile {
       pub(crate) fn load(path: &Path) -> anyhow::Result<Option<AgentsFile>>; // missing → None; parse error → "agents.toml is invalid"
       pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()>;          // toml::to_string + write_private_file
   }

   /// Where `cetra agents` reads and writes. Production values come from `Paths::detect`; tests pass temporary directories.
   #[derive(Debug, Clone)]
   pub(crate) struct Paths {
       pub(crate) home: PathBuf,       // dirs::home_dir()
       pub(crate) cetra_dir: PathBuf,  // dirs::config_dir()/cetra, the directory of credentials.toml
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

   /// `out` receives results, `err` notes. The hook never returns an error.
   pub(crate) async fn run(command: &AgentsCommand, paths: &Paths, out: &mut dyn Write, err: &mut dyn Write) -> anyhow::Result<()>;
   ```

3. **`main.rs`.**
   - Add `mod agents;`.
   - Add the `Command` variant:

     ```rust
     /// Connect coding agents (Claude Code, Codex, Gemini CLI, Copilot) to cetra.
     Agents { #[command(subcommand)] command: Box<agents::AgentsCommand> },
     ```

   - The dispatch arm builds stdout and stderr writers the way the `Projects` arm does, then:
     - if `command` is `AgentsCommand::Hook { agent }`: `match agents::Paths::detect() { Ok(paths) => agents::hook::hook(*agent, &paths, io::stdin(), &mut err).await, Err(_) => { let _ = writeln!(err, "cetra agents hook: cannot find the home directory"); } }`, then `Ok(())`;
     - otherwise: `agents::run(&command, &agents::Paths::detect()?, &mut out, &mut err).await?`.
4. **`agents/hook.rs`.**

   ```rust
   /// Runs the hook. Never fails and never writes to stdout; notes go to `err`.
   pub(crate) async fn hook(agent: Agent, paths: &Paths, stdin: impl Read + Send + 'static, err: &mut dyn Write)

   /// Runs `program args` with no stdin and the extra `envs`, captures stdout, and kills it at `timeout`.
   /// `None` on spawn failure, timeout, non-zero exit or non-UTF-8 output.
   pub(super) fn run_with_timeout(program: &str, args: &[&str], envs: &[(&str, &str)], timeout: Duration) -> Option<String>

   /// Removes `user[:password]@` from the authority of a `scheme://` URL. Other text is returned unchanged.
   pub(super) fn strip_user_info(remote: &str) -> String
   ```

   **`hook`, in order:**
   1. `let deadline = Instant::now() + Duration::from_secs(2);`
   2. On `cfg!(windows)`, return.
   3. Run steps 4–8 in `tokio::task::spawn_blocking(move || prepare(agent, paths_clone, stdin, deadline))`, which returns `Result<Prepared, &'static str>`. Here `Prepared { file: AgentsFile, body: serde_json::Value }`, and the `&'static str` is the note. A `JoinError` gives the note "hook failed".
   4. **Read stdin.** A `std::thread` reads stdin to EOF, at most 64 KiB (`take(65_536)`), and sends the bytes on a `std::sync::mpsc` channel. Wait with `recv_timeout(min(500 ms, time left))`. On a timeout: "no hook input".
   5. **Parse.** `struct HookInput { session_id: String, cwd: String }`; unknown fields are ignored. Failure gives "hook input is not valid JSON". A `session_id` that is empty or longer than 256 characters gives "hook input has no session id".
   6. **Load `AgentsFile`.** `None` gives "not set up; run cetra agents setup"; an error gives "agents.toml is invalid".
   7. **Repository and branch.**
      - repository: `run_with_timeout("git", &["-C", cwd, "remote", "get-url", "origin"], &[], min(1 s, time left))`, trimmed and passed through `strip_user_info`; empty or `None` gives `null`.
      - branch: the same with `&["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"]`, trimmed. `"HEAD"` (detached), empty or `None` gives `null`.
      - When no time is left, the value is `null` and git is not run.
   8. **Body.**

      ```json
      {"agent": "<wire name>", "session_id": "…", "repository": "…"|null, "branch": "…"|null}
      ```

      Add `"user": "…"` only when `AgentsFile.user` is set.
   9. **Post** (async, back in `hook`). If less than 100 ms is left, note "out of time" and return. Otherwise build a `reqwest::Client` with `connect_timeout` and `timeout` equal to the time left, and `redirect(Policy::none())`. `POST {url}/v1/agent-context` with `Authorization: Bearer {key}` and the JSON body. A non-2xx status gives `server answered <code>`; any send error gives "could not reach the server". Never format a `reqwest::Error`: it carries the URL.
   10. **Notes.** Each is one line to `err`: `let _ = writeln!(err, "cetra agents hook: {note}");`. A note never contains a URL, key, path, branch, email or the stdin content.

   **`run_with_timeout`:**
   - `std::process::Command::new(program)` with `.args(args)`, `.envs(envs.iter().copied())`, stdin and stderr null, and stdout piped. Spawn it.
   - Poll `try_wait` every 10 ms until `timeout`; then `kill` and `wait`, and return `None`.
   - After exit, read stdout to a string.

   **`strip_user_info`:**
   1. Find `://`; without it, return the input unchanged.
   2. The authority ends at the first `/`, `?` or `#` after `://`, or at the end of the string.
   3. If it contains `@`, remove everything from just after `://` through the **last** `@` of the authority.

**Tests** (`hook.rs` `mod tests`; process tests are `#[cfg(unix)]`):
- `strip_user_info`:
  - `https://tok@github.com/o/r.git` → `https://github.com/o/r.git`;
  - `https://u:p@ss@host/o/r` → `https://host/o/r`;
  - `https://host/o?x=a@b` unchanged;
  - `git@github.com:o/r.git` unchanged;
  - `https://github.com/o/r` unchanged;
  - `/local/path` unchanged.
- `run_with_timeout`:
  - `("sleep", &["5"], &[], 100 ms)` returns `None` within 1 s;
  - `("echo", &["hi"], &[], 1 s)` returns `Some("hi\n")`;
  - a missing program returns `None`;
  - `("sh", &["-c", "echo $X"], &[("X", "y")], 1 s)` returns `Some("y\n")`.
- `hook_posts_repository_and_branch`:
  - Setup: a temp git repo built with `run_with_timeout` or `Command`, all with `GIT_CONFIG_NOSYSTEM=1` and `HOME=<tempdir>`:
    - `git init -b feature/x`;
    - `git -c user.name=t -c user.email=t@example.com commit --allow-empty -m i`;
    - `git remote add origin https://tok@github.com/Acme/Widgets.git`.
  - A `Canned::json(vec![(204, String::new())])` server, and `agents.toml` with its URL, key `sk-cetra-test` and user `dev@example.com`.
  - Run `hook(Agent::ClaudeCode, …)` with stdin `{"session_id":"s1","cwd":"<repo>","hook_event_name":"SessionStart"}`.
  - The recorded request's `head` starts with `POST /v1/agent-context` and contains `authorization: Bearer sk-cetra-test`. Its body, parsed, equals `{"agent":"claude-code","session_id":"s1","repository":"https://github.com/Acme/Widgets.git","branch":"feature/x","user":"dev@example.com"}`.
  - `err` is empty.
- `hook_sends_null_outside_git`: `cwd` is a plain temp dir. The body has `"repository":null,"branch":null`, and no `user` key when `AgentsFile.user` is `None`.
- `hook_sends_null_branch_when_detached`: after `git checkout --detach`, `branch` is `null`.
- `hook_without_agents_toml_posts_nothing`: nothing is recorded, and `err` has the "not set up" note.
- `hook_with_bad_input_posts_nothing`: invalid JSON, and JSON without `session_id`.
- `hook_returns_within_its_budget_when_the_server_is_down`: `url` points to a closed port; the function returns within 2.5 s.
- `hook_through_run_cli_exits_zero`: `run_cli(["cetra", "agents", "hook", "codex"])` with stdin from the test process returns `Ok(())`. `Paths::detect` may find a real home, but nothing is posted without `agents.toml` there. If the test machine's home might hold an `agents.toml`, skip the test when `Paths::detect()?.agents_file()` exists.
- `agents_file_round_trips_with_mode_0600` (unix), and `agents_file_debug_hides_the_key`.
- `write_private_file_keeps_an_existing_directory_mode` (unix): a directory with mode 0755 keeps 0755.
- The existing `credentials` tests still pass.

**Done when:**
- `cargo test -p cetra-cli agents` passes.
- `cargo test -p cetra-cli credentials` passes.
- `cargo clippy -p cetra-cli --all-targets -- -D warnings` passes.

**Commit:** `Add the cetra agents hook command`

---

## A2 — `setup` and `remove` for the four agents

**Goal:** `cetra agents setup <agent>` and `remove <agent>` for Claude Code, Codex, Gemini CLI and Copilot.

**Edit:**
- `Cargo.toml` (root: `toml_edit`)
- `crates/cli/Cargo.toml`
- `Cargo.lock`
- `crates/cli/src/agents/mod.rs`
- `crates/cli/src/agents/setup.rs` (new)
- `crates/cli/src/agents/codex.rs` (new)

**Steps:**
1. **Dependency.** Add `toml_edit` as in § Versions.
2. **CLI surface** in `mod.rs`. Add `mod setup; mod codex;`, then these variants:

   ```rust
   /// Configure an agent to send its traces to cetra (writes the agent's user settings).
   Setup(SetupArgs),
   /// Remove what `cetra agents setup` wrote for an agent.
   Remove { agent: Agent },
   ```

   ```rust
   #[derive(Debug, clap::Args)]
   pub(crate) struct SetupArgs {
       pub(crate) agent: Agent,
       /// cetra server URL, for example `https://cetra.example.com`.
       #[arg(long)]
       pub(crate) url: String,
       /// Project secret key with the `ingest` scope. A value given as a flag is visible to other local users in the process list; prefer CETRA_INGEST_KEY.
       #[arg(long, env = "CETRA_INGEST_KEY", hide_env_values = true)]
       pub(crate) key: crate::client::Token,
       /// The person to attribute sessions to (default: `git config --global user.email`).
       #[arg(long)]
       pub(crate) user: Option<String>,
       /// Do not turn on prompt and tool content capture.
       #[arg(long)]
       pub(crate) no_content: bool,
       /// Print the settings, including the key, instead of writing them.
       #[arg(long)]
       pub(crate) print: bool,
   }
   ```

   `run` dispatches `Setup` to `setup::setup(args, paths, out, err)` and `Remove` to `setup::remove(agent, paths, out, err)`. If `Token` does not implement what clap's value parser needs (`FromStr` plus `Clone`), stop and report.
3. **`setup`** (`setup.rs`), in order:
   1. On `cfg!(windows)`: error "cetra agents is not supported on Windows".
   2. `let url = credentials::server_url(&args.url)?;`
   3. `args.key` must start with `sk-cetra-`, else the error "the key must be a project secret key (sk-cetra-…) with the ingest scope". The error never echoes the key.
   4. **Copilot** goes straight to `copilot()` (step 7): no `agents.toml`, no git lookup.
   5. `user`:
      - `--user` if given;
      - else `hook::run_with_timeout("git", &["config", "--global", "user.email"], &[], 1 s)`;
      - trimmed and lowercased; empty becomes `None`.
   6. Unless `--print`, save `AgentsFile { url, key, user }` to `paths.agents_file()`.
   7. Call the agent function: `claude_code`, `codex::codex`, `gemini_cli` or `copilot`. Each writes `wrote <path>` lines to `out` for the files it wrote, then its notes, one per line.
4. **`remove`**:
   1. On Windows, the same error as `setup`.
   2. `AgentsFile::load`. When it is `None`, the telemetry keys are left as they are, with the note "agents.toml not found; telemetry settings left unchanged", but our hooks are still removed.
   3. Call the agent's remove function. It writes `updated <path>` for each changed file, or `nothing to remove`.
5. **Shared JSON helpers** (`setup.rs`):

   ```rust
   fn hook_command(paths: &Paths, agent: Agent) -> String; // "<shell_quote(exe)> agents hook <wire name>"
   fn shell_quote(path: &Path) -> String;
   fn is_our_hook(command: &str, agent: Agent) -> bool;   // command.ends_with(&format!("agents hook {}", agent.wire_name()))
   fn set_hooks(root: &mut serde_json::Map<String, Value>, events: &[&str], command: &str, agent: Agent);
   fn remove_hooks(root: &mut serde_json::Map<String, Value>, agent: Agent) -> bool; // true when something was removed
   fn read_json_object(path: &Path) -> anyhow::Result<serde_json::Map<String, Value>>;
   fn write_settings(path: &Path, bytes: &[u8]) -> anyhow::Result<()>;
   fn backup_once(path: &Path) -> anyhow::Result<()>;
   ```

   - **`shell_quote`.** The path as is when every character is in `[A-Za-z0-9/._+:@%=,-]`. Otherwise single quotes around it, with each `'` written as `'\''`.
   - **`set_hooks`.** For each event:
     1. `root["hooks"][event]` is an array, created empty if absent.
     2. In every element, remove the inner `hooks` entries whose `command` satisfies `is_our_hook`; drop an element whose inner `hooks` array is then empty.
     3. Append `{"hooks": [{"type": "command", "command": <command>, "timeout": 5}]}`.
     4. Everything else keeps its value and order.
   - **`remove_hooks`.** The same removal over every event. Then drop events left with an empty array, and drop `"hooks"` if it is left empty.
   - **`read_json_object`.** A missing file is an empty map. Invalid JSON, or JSON that is not an object, is the error "<path> is not valid JSON; fix it or run with --print".
   - **`write_settings`.**
     1. Resolve symlinks: `fs::canonicalize` when the path exists.
     2. `backup_once`.
     3. `credentials::write_private_file`. The file ends with `"\n"`; for JSON, it is `serde_json::to_vec_pretty` (2 spaces). Key order is kept: the workspace enables `preserve_order`.
   - **`backup_once`.** If the file exists and `<file>.cetra-backup` does not, copy the file there.
   - A file is written only when its new bytes differ from the current ones.
6. **Claude Code**, file `<home>/.claude/settings.json`.
   - **Setup.**
     - `env` is an object, created if absent. Set, as JSON strings:
       - `CLAUDE_CODE_ENABLE_TELEMETRY=1`
       - `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`
       - `OTEL_TRACES_EXPORTER=otlp`
       - `OTEL_LOGS_EXPORTER=none`
       - `OTEL_METRICS_EXPORTER=none`
       - `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`
       - `OTEL_EXPORTER_OTLP_ENDPOINT=<url>`
       - `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`
       - `OTEL_METRICS_INCLUDE_REPOSITORY=true`
     - With content, also set `OTEL_LOG_USER_PROMPTS=1`, `OTEL_LOG_TOOL_DETAILS=1` and `OTEL_LOG_TOOL_CONTENT=1`. With `--no-content`, remove those three.
     - Then `set_hooks(root, &["SessionStart", "UserPromptSubmit", "CwdChanged"], …)`.
     - Note: "Start a new Claude Code session to apply the settings."
   - **Remove.** When `env.OTEL_EXPORTER_OTLP_ENDPOINT` equals the `agents.toml` URL, remove all twelve keys above and drop `env` if it is left empty. Otherwise note "the telemetry settings point to another server; left unchanged". Then `remove_hooks`.
7. **Copilot.** It writes nothing, with or without `--print`, and prints:

   ```
   # Copilot CLI: add to your shell profile (Copilot CLI only exports to https:// endpoints)
   export COPILOT_OTEL_ENABLED=true
   export OTEL_EXPORTER_OTLP_ENDPOINT=<url>
   export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
   export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer%20<key>'
   export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY
   
   # VS Code: add to settings.json; the key comes from OTEL_EXPORTER_OTLP_HEADERS in VS Code's environment
   {
     "github.copilot.chat.otel.enabled": true,
     "github.copilot.chat.otel.exporterType": "otlp-http",
     "github.copilot.chat.otel.otlpEndpoint": "<url>",
     "github.copilot.chat.otel.captureContent": true
   }
   ```

   - With `--no-content`, the `CAPTURE_CONTENT` line is left out and `captureContent` is `false`.
   - When the URL starts with `http://`, also print "Copilot CLI does not export to http:// endpoints; put TLS in front of cetra."
   - `remove copilot` prints "nothing to remove: cetra agents setup copilot writes no file".
8. **Codex** (`codex.rs`). The Codex home is `$CODEX_HOME` when set and non-empty, else `<home>/.codex`. `setup` and `remove` read the variable and pass the path in, so tests never set it.
   - **Setup, `config.toml`.**
     - Parse it with `toml_edit::DocumentMut`. A missing file is empty; invalid TOML is the error "<path> is not valid TOML; fix it or run with --print".
     - Ensure `[otel]` is a table, and set:
       - `exporter = "none"`
       - `metrics_exporter = "none"`
       - `trace_exporter = { otlp-http = { endpoint = "<url>/v1/traces", protocol = "binary", headers = { authorization = "Bearer <key>" } } }`, as nested `toml_edit::InlineTable` values
     - Leave other keys and comments as they were. Write with `write_settings`.
   - **Setup, `hooks.json`.** `<codex home>/hooks.json`: `set_hooks(&["SessionStart", "UserPromptSubmit"], …)`.
   - `--no-content` changes nothing: Codex spans carry no content.
   - Note: "Codex runs the cetra hook only after you trust it in Codex; until then, traces have no repository or branch."
   - **Remove.**
     - When `otel.trace_exporter.otlp-http.endpoint` equals `<agents.toml url>/v1/traces`: remove `trace_exporter`, then `exporter` and `metrics_exporter` when their value is `"none"` (a value setup replaced is not restored), and drop `[otel]` if it is left empty.
     - Otherwise, the "another server" note.
     - Then remove our hooks from `hooks.json`.
   - **`--print`** prints both files' new contents, each under a `# <path>` line.
9. **Gemini CLI** (`setup.rs`).
   - **Setup, `settings.json`.**
     - In `<home>/.gemini/settings.json`, set in the `telemetry` object: `enabled: true`, `traces: true`, `target: "local"`, `otlpEndpoint: "<url>"`, `otlpProtocol: "http"`, and `logPrompts: true` (`false` with `--no-content`).
     - Keep the other `telemetry` keys.
     - Then `set_hooks(&["SessionStart", "BeforeAgent"], …)`.
   - **Setup, `.env`.**
     - Read `<home>/.gemini/.env`; a missing file is empty. Split it on `\n`; a line keeps its `\r` if it had one.
     - Replace the first line that starts with `OTEL_EXPORTER_OTLP_HEADERS=` with `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<key>`, or append it.
     - Keep the other lines in order, and end with `\n`. Write with `write_settings`, so the mode is 0600.
   - Note: "Gemini CLI also sends logs and metrics; cetra accepts and drops them."
   - **Remove.**
     - When `telemetry.otlpEndpoint` equals the `agents.toml` URL: remove the six keys above, and drop `telemetry` if it is left empty.
     - Remove the `.env` line `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20<agents.toml key>` when it matches exactly.
     - Then remove our hooks.
10. **`--print`** for Claude Code and Gemini CLI prints each file's merged content under a `# <path>` line. It writes nothing and saves no `agents.toml`.

**Tests** (in `setup.rs` and `codex.rs`, on temporary `Paths` with `exe = /opt/cetra/bin/cetra`; every test passes `--user`, except the two that check the git default):
- **`shell_quote`:** keeps `/opt/cetra/bin/cetra`, and quotes `/Users/a b/cetra` and `/tmp/it's/cetra`.
- **Claude Code:**
  - `claude_setup_on_empty_home`: exactly the env keys and three hook events; `agents.toml` saved with mode 0600.
  - `claude_setup_keeps_other_settings`:
    - The existing file is `{"model": "x", "env": {"FOO": "1"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other"}]}], "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": "mine"}]}]}}`.
    - After setup, `model`, `FOO`, the `Stop` hook and the `mine` entry are kept, and our entry is appended.
    - The backup equals the original bytes.
  - `claude_setup_twice_is_idempotent`: identical bytes, one entry of ours per event, the backup unchanged.
  - `claude_setup_no_content_removes_content_flags`.
  - `claude_setup_rejects_invalid_json_and_writes_nothing`.
  - `claude_print_writes_nothing`.
  - `claude_setup_follows_a_symlink` (unix): `settings.json` is a link to `dotfiles/claude.json`; the target is updated and the link remains.
  - `claude_remove_restores_the_other_settings`: setup then remove gives a `serde_json::Value` equal to the original.
  - `claude_remove_twice`: the second run writes nothing and prints "nothing to remove".
  - `claude_remove_leaves_env_pointing_elsewhere`.
  - `claude_remove_without_agents_toml_removes_hooks_only`.
- **Codex:**
  - `codex_setup_keeps_comments_and_other_keys`: a comment line, `model = "x"` and `[profiles.a]` are unchanged in the text, and `[otel]` has the three keys.
  - `codex_setup_twice_is_idempotent` (byte-identical).
  - `codex_remove_restores_the_text`: setup on a file without `[otel]`, then remove, gives exactly the original text. If `toml_edit` cannot round-trip it, stop and report the difference.
  - `codex_hooks_json_has_two_events`.
  - `codex_home_parameter_is_used`.
- **Gemini CLI:**
  - `gemini_setup_writes_settings_and_env`: an existing `.env` with `GEMINI_API_KEY=x\r\n` keeps that line byte for byte.
  - `gemini_setup_twice_is_idempotent`.
  - `gemini_remove_restores`.
  - `gemini_no_content_sets_log_prompts_false`.
- **Copilot:** `copilot_prints_and_writes_nothing` (no `agents.toml`), `copilot_no_content`, `copilot_http_note`.
- **Common:**
  - `setup_refuses_a_non_sk_key`: the message does not contain the key.
  - `setup_refuses_a_bad_url`.
  - `user_defaults_to_git_config`: a temp `HOME` holding a `.gitconfig` with `user.email = Dev@Example.com`, passed to git through `run_with_timeout`'s `envs`. Give the user lookup an inner function that takes the extra `envs`. The result is `dev@example.com`.

**Done when:**
- `cargo test -p cetra-cli agents` passes.
- `cargo clippy -p cetra-cli --all-targets -- -D warnings` passes.

**Commit:** `Add cetra agents setup and remove`

---

## A3 — Docs: spec 09, the CLI reference, the changelog

**Goal:** the generated CLI reference, spec 09 and the changelog list `cetra agents`.

**Edit:**
- `crates/cli/src/main.rs` (the intro sentence in `cli_reference` only)
- `website/src/content/docs/docs/reference/cli.md` (generated)
- `docs/spec/09-api.md`
- `CHANGELOG.md`

**Steps:**
1. In `cli_reference`, the intro sentence listing the remote commands adds `` `cetra agents` ``.
2. Run `CETRA_DOCS_WRITE=1 cargo test -p cetra-cli cli_reference_is_up_to_date`. The page gains `agents setup` and `agents remove`, with `[env: CETRA_INGEST_KEY]`. Check that `agents hook` does not appear.
3. Spec 09 § CLI: add these rows after `projects list`:
   - `` | `cetra agents setup <agent> --url` | configure Claude Code, Codex, Gemini CLI or Copilot to send traces to cetra; key from `CETRA_INGEST_KEY` or `--key`; `--user`, `--no-content`, `--print` (D-CA-10) | ``
   - `` | `cetra agents remove <agent>` | remove what setup wrote (D-CA-10) | ``

   Add the sentence: "`cetra agents hook <agent>` is hidden: the agents' hooks run it to send the session context (D-CA-8)."
4. `CHANGELOG.md` § `[Unreleased]` › `### Added`: `` - `cetra agents setup` and `cetra agents remove` connect Claude Code, Codex, Gemini CLI and Copilot to cetra, with a hook that reports each session's repository, branch and person. ``
5. `docs/roadmap.md` is not changed: the coordinator updates the CA row.

**Done when:**
- `cargo test -p cetra-cli` passes.
- `export PATH=…; just check` passes.

**Commit:** `Document cetra agents in the CLI reference, spec 09 and the changelog`
