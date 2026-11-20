//! `cetra agents setup` and `remove` (D-CA-10): writes each agent's user-level settings so its
//! traces reach cetra, and removes what setup wrote. Claude Code and Gemini CLI are JSON files
//! here, Codex is in `codex.rs`, and Copilot has no file: setup prints its settings instead.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::codex;
use super::hook::run_with_timeout;
use super::{Agent, AgentsFile, Paths, SetupArgs};
use crate::credentials::{self, write_private_file};

/// Claude Code's `env` keys that content capture adds; `--no-content` removes them.
const CLAUDE_CONTENT: [&str; 3] = [
    "OTEL_LOG_USER_PROMPTS",
    "OTEL_LOG_TOOL_DETAILS",
    "OTEL_LOG_TOOL_CONTENT",
];
const CLAUDE_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "CwdChanged"];
const GEMINI_EVENTS: [&str; 2] = ["SessionStart", "BeforeAgent"];
/// The `telemetry` keys of Gemini's `settings.json` that setup sets.
const GEMINI_TELEMETRY: [&str; 6] = [
    "enabled",
    "traces",
    "target",
    "otlpEndpoint",
    "otlpProtocol",
    "logPrompts",
];
const HEADERS_PREFIX: &str = "OTEL_EXPORTER_OTLP_HEADERS=";
pub(super) const NOT_FOUND_NOTE: &str = "agents.toml not found; telemetry settings left unchanged";
pub(super) const OTHER_SERVER_NOTE: &str =
    "the telemetry settings point to another server; left unchanged";

/// `cetra agents setup <agent>`. Results and notes go to `out`.
pub(crate) fn setup(args: &SetupArgs, paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    refuse_on_windows()?;
    let url = credentials::server_url(&args.url)?;
    let key = args.key.expose();
    if !key.starts_with("sk-cetra-") {
        anyhow::bail!("the key must be a project secret key (sk-cetra-…) with the ingest scope");
    }
    if args.agent != Agent::Copilot && !args.print {
        let user = user_for(args.user.as_deref(), &[]);
        AgentsFile {
            url: url.clone(),
            key: key.to_owned(),
            user,
        }
        .save(&paths.agents_file())?;
    }
    match args.agent {
        Agent::ClaudeCode => claude_code_setup(args, &url, paths, out),
        Agent::Codex => codex::setup(args, &url, paths, &codex::codex_home(paths), out),
        Agent::GeminiCli => gemini_cli_setup(args, &url, paths, out),
        Agent::Copilot => copilot(args, &url, out),
    }
}

/// `cetra agents remove <agent>`. Results and notes go to `out`.
pub(crate) fn remove(agent: Agent, paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    refuse_on_windows()?;
    match agent {
        Agent::ClaudeCode => claude_code_remove(paths, out),
        Agent::Codex => codex::remove(paths, &codex::codex_home(paths), out),
        Agent::GeminiCli => gemini_cli_remove(paths, out),
        Agent::Copilot => {
            writeln!(
                out,
                "nothing to remove: cetra agents setup copilot writes no file"
            )?;
            Ok(())
        }
    }
}

fn refuse_on_windows() -> anyhow::Result<()> {
    if cfg!(windows) {
        anyhow::bail!("cetra agents is not supported on Windows");
    }
    Ok(())
}

/// The person to attribute sessions to: `--user`, else `git config --global user.email`.
/// Trimmed and lowercased; empty is `None`. `envs` reaches git (tests set `HOME`).
fn user_for(flag: Option<&str>, envs: &[(&str, &str)]) -> Option<String> {
    match flag {
        Some(user) => normalize_user(user),
        None => run_with_timeout(
            "git",
            &["config", "--global", "user.email"],
            envs,
            Duration::from_secs(1),
        )
        .and_then(|user| normalize_user(&user)),
    }
}

fn normalize_user(raw: &str) -> Option<String> {
    let user = raw.trim().to_lowercase();
    (!user.is_empty()).then_some(user)
}

/// `hook <agent>` as the agent runs it, with the absolute executable path (D-CA-10).
pub(super) fn hook_command(paths: &Paths, agent: Agent) -> String {
    format!(
        "{} agents hook {}",
        shell_quote(&paths.exe),
        agent.wire_name()
    )
}

/// The path as is when every character is safe in a shell word; otherwise single-quoted.
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+:@%=,-".contains(c));
    if plain {
        return text.into_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn is_our_hook(command: &str, agent: Agent) -> bool {
    command.ends_with(&format!("agents hook {}", agent.wire_name()))
}

/// Sets one hook per event, after removing our earlier entries for the agent. Other entries
/// keep their value and order.
pub(super) fn set_hooks(
    root: &mut Map<String, Value>,
    events: &[&str],
    command: &str,
    agent: Agent,
) -> anyhow::Result<()> {
    let hooks = object_entry(root, "hooks")?;
    for event in events {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("the {event} hooks setting is not an array"))?;
        remove_ours(entries, agent);
        entries.push(json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]}));
    }
    Ok(())
}

/// Removes our hooks from every event. Events emptied by that are dropped, and so is `hooks`
/// when it is left empty.
pub(super) fn remove_hooks(root: &mut Map<String, Value>, agent: Agent) -> anyhow::Result<()> {
    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object_mut()
        .context("the hooks setting is not an object")?;
    let mut emptied = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        if remove_ours(entries, agent) && entries.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in &emptied {
        hooks.remove(event);
    }
    if !emptied.is_empty() && hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(())
}

/// Removes our inner hook entries from each element, and an element left with none. True when
/// something was removed.
fn remove_ours(entries: &mut Vec<Value>, agent: Agent) -> bool {
    let mut removed = false;
    entries.retain_mut(|element| {
        let Some(inner) = element.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = inner.len();
        inner.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| is_our_hook(command, agent))
        });
        if inner.len() == before {
            return true;
        }
        removed = true;
        !inner.is_empty()
    });
    removed
}

/// The object at `map[key]`, created when absent.
pub(super) fn object_entry<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> anyhow::Result<&'a mut Map<String, Value>> {
    map.entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .with_context(|| format!("the {key} setting is not an object"))
}

/// Whether `section[key]` is the string `url`.
fn points_at(section: Option<&Value>, key: &str, url: &str) -> bool {
    section
        .and_then(|section| section.get(key))
        .and_then(Value::as_str)
        == Some(url)
}

/// A JSON settings file as an object. A missing file is empty.
pub(super) fn read_json_object(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let Some(text) = read_optional_text(path)? else {
        return Ok(Map::new());
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(root)) => Ok(root),
        _ => anyhow::bail!(
            "{} is not valid JSON; fix it or run with --print",
            path.display()
        ),
    }
}

/// The text of a file, or `None` when it is missing.
pub(super) fn read_optional_text(path: &Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => anyhow::bail!("reading {}", path.display()),
    }
}

/// Pretty JSON with two spaces and a final newline. Key order is kept (`preserve_order`).
pub(super) fn settings_bytes(root: &Map<String, Value>) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(root).context("encoding the settings")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes a settings file with mode 0600, following a symlink to its target. The first write
/// of a file keeps a copy beside it, `<file>.cetra-backup`. True when the file changed.
pub(super) fn write_settings(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    let target = if path.exists() {
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    match fs::read(&target) {
        Ok(current) if current == bytes => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("reading {}", path.display()),
    }
    backup_once(&target)?;
    write_private_file(&target, bytes)?;
    Ok(true)
}

/// Copies an existing file to `<file>.cetra-backup` unless that backup already exists.
fn backup_once(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut name = path
        .file_name()
        .context("the settings path has no file name")?
        .to_os_string();
    name.push(".cetra-backup");
    let backup = path.with_file_name(name);
    if backup.exists() {
        return Ok(());
    }
    let bytes = fs::read(path).context("reading the settings file for its backup")?;
    write_private_file(&backup, &bytes)
}

/// Prints a file's new contents under a `# <path>` line (`--print`).
fn print_file(out: &mut dyn Write, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    writeln!(out, "# {}", path.display())?;
    out.write_all(bytes)?;
    Ok(())
}

/// Writes a file for setup, or prints it under `--print`. Reports `wrote <path>` on a change.
pub(super) fn emit(
    path: &Path,
    bytes: &[u8],
    print: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if print {
        return print_file(out, path, bytes);
    }
    if write_settings(path, bytes)? {
        writeln!(out, "wrote {}", path.display())?;
    }
    Ok(())
}

/// Writes a file for remove. Reports `updated <path>` on a change and returns whether it changed.
pub(super) fn updated(path: &Path, bytes: &[u8], out: &mut dyn Write) -> anyhow::Result<bool> {
    let changed = write_settings(path, bytes)?;
    if changed {
        writeln!(out, "updated {}", path.display())?;
    }
    Ok(changed)
}

/// The Claude Code telemetry keys that setup sets (D-CA-10).
fn claude_telemetry(url: &str, key: &str) -> [(&'static str, String); 9] {
    [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1".to_owned()),
        ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1".to_owned()),
        ("OTEL_TRACES_EXPORTER", "otlp".to_owned()),
        ("OTEL_LOGS_EXPORTER", "none".to_owned()),
        ("OTEL_METRICS_EXPORTER", "none".to_owned()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".to_owned()),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", url.to_owned()),
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("Authorization=Bearer%20{key}"),
        ),
        ("OTEL_METRICS_INCLUDE_REPOSITORY", "true".to_owned()),
    ]
}

fn claude_settings_path(paths: &Paths) -> PathBuf {
    paths.home.join(".claude").join("settings.json")
}

fn claude_code_setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let path = claude_settings_path(paths);
    let mut root = read_json_object(&path)?;
    let env = object_entry(&mut root, "env")?;
    for (name, value) in claude_telemetry(url, args.key.expose()) {
        env.insert(name.to_owned(), Value::String(value));
    }
    for name in CLAUDE_CONTENT {
        if args.no_content {
            env.remove(name);
        } else {
            env.insert(name.to_owned(), json!("1"));
        }
    }
    set_hooks(
        &mut root,
        &CLAUDE_EVENTS,
        &hook_command(paths, Agent::ClaudeCode),
        Agent::ClaudeCode,
    )?;
    emit(&path, &settings_bytes(&root)?, args.print, out)?;
    if !args.print {
        writeln!(
            out,
            "Start a new Claude Code session to apply the settings."
        )?;
    }
    Ok(())
}

fn claude_code_remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    let agents = AgentsFile::load(&paths.agents_file())?;
    let path = claude_settings_path(paths);
    let mut root = read_json_object(&path)?;
    let original = root.clone();
    match &agents {
        None => writeln!(out, "{NOT_FOUND_NOTE}")?,
        Some(file) => {
            if points_at(root.get("env"), "OTEL_EXPORTER_OTLP_ENDPOINT", &file.url) {
                if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
                    for (name, _) in claude_telemetry(&file.url, &file.key) {
                        env.remove(name);
                    }
                    for name in CLAUDE_CONTENT {
                        env.remove(name);
                    }
                    if env.is_empty() {
                        root.remove("env");
                    }
                }
            } else if root
                .get("env")
                .and_then(|env| env.get("OTEL_EXPORTER_OTLP_ENDPOINT"))
                .is_some()
            {
                writeln!(out, "{OTHER_SERVER_NOTE}")?;
            }
        }
    }
    remove_hooks(&mut root, Agent::ClaudeCode)?;
    let mut changed = false;
    if root != original {
        changed |= updated(&path, &settings_bytes(&root)?, out)?;
    }
    if !changed {
        writeln!(out, "nothing to remove")?;
    }
    Ok(())
}

fn gemini_dir(paths: &Paths) -> PathBuf {
    paths.home.join(".gemini")
}

/// The `.env` line that Gemini CLI reads its OTLP headers from.
fn gemini_header_line(key: &str) -> String {
    format!("{HEADERS_PREFIX}Authorization=Bearer%20{key}")
}

/// Splits text into lines without their `\n`. A final newline does not make an empty line.
fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Joins lines, each ended with `\n`.
fn join_lines(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// The `.env` text with its headers line set to `line`. The first such line is replaced and
/// keeps its `\r`; otherwise `line` is appended. Other lines keep their bytes and order.
fn env_with_header(text: &str, line: &str) -> String {
    let mut lines = split_lines(text);
    match lines
        .iter()
        .position(|kept| kept.starts_with(HEADERS_PREFIX))
    {
        Some(index) => {
            let carriage = if lines[index].ends_with('\r') {
                "\r"
            } else {
                ""
            };
            lines[index] = format!("{line}{carriage}");
        }
        None => lines.push(line.to_owned()),
    }
    join_lines(&lines)
}

/// The `.env` text without the lines equal to `line` (a trailing `\r` ignored), or `None` when
/// there is no such line.
fn env_without_line(text: &str, line: &str) -> Option<String> {
    let mut lines = split_lines(text);
    let before = lines.len();
    lines.retain(|kept| kept.trim_end_matches('\r') != line);
    (lines.len() != before).then(|| join_lines(&lines))
}

fn gemini_cli_setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let dir = gemini_dir(paths);
    let settings_path = dir.join("settings.json");
    let env_path = dir.join(".env");
    let mut root = read_json_object(&settings_path)?;
    let telemetry = object_entry(&mut root, "telemetry")?;
    telemetry.insert("enabled".to_owned(), Value::Bool(true));
    telemetry.insert("traces".to_owned(), Value::Bool(true));
    telemetry.insert("target".to_owned(), json!("local"));
    telemetry.insert("otlpEndpoint".to_owned(), json!(url));
    telemetry.insert("otlpProtocol".to_owned(), json!("http"));
    telemetry.insert("logPrompts".to_owned(), Value::Bool(!args.no_content));
    set_hooks(
        &mut root,
        &GEMINI_EVENTS,
        &hook_command(paths, Agent::GeminiCli),
        Agent::GeminiCli,
    )?;
    let env_text = read_optional_text(&env_path)?.unwrap_or_default();
    let env = env_with_header(&env_text, &gemini_header_line(args.key.expose()));
    emit(&settings_path, &settings_bytes(&root)?, args.print, out)?;
    emit(&env_path, env.as_bytes(), args.print, out)?;
    if !args.print {
        writeln!(
            out,
            "Gemini CLI also sends logs and metrics; cetra accepts and drops them."
        )?;
    }
    Ok(())
}

fn gemini_cli_remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    let agents = AgentsFile::load(&paths.agents_file())?;
    let dir = gemini_dir(paths);
    let settings_path = dir.join("settings.json");
    let env_path = dir.join(".env");
    let mut root = read_json_object(&settings_path)?;
    let original = root.clone();
    match &agents {
        None => writeln!(out, "{NOT_FOUND_NOTE}")?,
        Some(file) => {
            if points_at(root.get("telemetry"), "otlpEndpoint", &file.url) {
                if let Some(telemetry) = root.get_mut("telemetry").and_then(Value::as_object_mut) {
                    for name in GEMINI_TELEMETRY {
                        telemetry.remove(name);
                    }
                    if telemetry.is_empty() {
                        root.remove("telemetry");
                    }
                }
            } else if root
                .get("telemetry")
                .and_then(|telemetry| telemetry.get("otlpEndpoint"))
                .is_some()
            {
                writeln!(out, "{OTHER_SERVER_NOTE}")?;
            }
        }
    }
    remove_hooks(&mut root, Agent::GeminiCli)?;
    let mut changed = false;
    if root != original {
        changed |= updated(&settings_path, &settings_bytes(&root)?, out)?;
    }
    if let Some(file) = &agents
        && let Some(text) = read_optional_text(&env_path)?
        && let Some(kept) = env_without_line(&text, &gemini_header_line(&file.key))
    {
        changed |= updated(&env_path, kept.as_bytes(), out)?;
    }
    if !changed {
        writeln!(out, "nothing to remove")?;
    }
    Ok(())
}

/// Copilot has no settings file: setup prints the variables and the VS Code settings. Nothing
/// is written, with or without `--print`.
fn copilot(args: &SetupArgs, url: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let capture = !args.no_content;
    writeln!(
        out,
        "# Copilot CLI: add to your shell profile (Copilot CLI only exports to https:// endpoints)"
    )?;
    writeln!(out, "export COPILOT_OTEL_ENABLED=true")?;
    writeln!(out, "export OTEL_EXPORTER_OTLP_ENDPOINT={url}")?;
    writeln!(out, "export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf")?;
    writeln!(
        out,
        "export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer%20{}'",
        args.key.expose()
    )?;
    if capture {
        writeln!(
            out,
            "export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY"
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "# VS Code: add to settings.json; the key comes from OTEL_EXPORTER_OTLP_HEADERS in VS Code's environment"
    )?;
    writeln!(out, "{{")?;
    writeln!(out, "  \"github.copilot.chat.otel.enabled\": true,")?;
    writeln!(
        out,
        "  \"github.copilot.chat.otel.exporterType\": \"otlp-http\","
    )?;
    writeln!(
        out,
        "  \"github.copilot.chat.otel.otlpEndpoint\": \"{url}\","
    )?;
    writeln!(
        out,
        "  \"github.copilot.chat.otel.captureContent\": {capture}"
    )?;
    writeln!(out, "}}")?;
    if url.starts_with("http://") {
        writeln!(
            out,
            "Copilot CLI does not export to http:// endpoints; put TLS in front of cetra."
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::client::Token;

    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            cetra_dir: root.join("cetra"),
            exe: PathBuf::from("/opt/cetra/bin/cetra"),
        }
    }

    fn setup_args(agent: Agent) -> SetupArgs {
        SetupArgs {
            agent,
            url: "https://cetra.example.com/".to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: Some("Dev@Example.com".to_owned()),
            no_content: false,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, paths: &Paths) -> String {
        let mut out = Vec::new();
        setup(args, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove(agent: Agent, paths: &Paths) -> String {
        let mut out = Vec::new();
        remove(agent, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn claude_path(paths: &Paths) -> PathBuf {
        claude_settings_path(paths)
    }

    fn backup_of(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.cetra-backup", path.display()))
    }

    fn write_file(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn save_agents(paths: &Paths, url: &str) {
        AgentsFile {
            url: url.to_owned(),
            key: "sk-cetra-test".to_owned(),
            user: None,
        }
        .save(&paths.agents_file())
        .unwrap();
    }

    #[test]
    fn shell_quote_keeps_plain_paths_and_quotes_the_rest() {
        assert_eq!(
            shell_quote(Path::new("/opt/cetra/bin/cetra")),
            "/opt/cetra/bin/cetra"
        );
        assert_eq!(
            shell_quote(Path::new("/Users/a b/cetra")),
            "'/Users/a b/cetra'"
        );
        assert_eq!(
            shell_quote(Path::new("/tmp/it's/cetra")),
            "'/tmp/it'\\''s/cetra'"
        );
    }

    #[test]
    fn user_is_trimmed_and_lowercased() {
        assert_eq!(user_for(Some("  A@B.c "), &[]), Some("a@b.c".to_owned()));
        assert_eq!(user_for(Some("  "), &[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn user_defaults_to_git_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitconfig"),
            "[user]\n\temail = Dev@Example.com\n",
        )
        .unwrap();
        let home = dir.path().to_str().unwrap();
        assert_eq!(
            user_for(None, &[("HOME", home), ("GIT_CONFIG_NOSYSTEM", "1")]),
            Some("dev@example.com".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_on_empty_home() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        let settings = json_at(&claude_path(&paths));
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env.len(), 12);
        assert_eq!(
            env["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "https://cetra.example.com"
        );
        assert_eq!(
            env["OTEL_EXPORTER_OTLP_HEADERS"],
            "Authorization=Bearer%20sk-cetra-test"
        );
        assert_eq!(env["OTEL_LOG_TOOL_CONTENT"], "1");
        assert_eq!(settings["hooks"].as_object().unwrap().len(), 3);
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "/opt/cetra/bin/cetra agents hook claude-code"
        );
        let agents = AgentsFile::load(&paths.agents_file()).unwrap().unwrap();
        assert_eq!(agents.user.as_deref(), Some("dev@example.com"));
        let mode = fs::metadata(paths.agents_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let path = claude_path(&paths);
        let original = br#"{"model": "x", "env": {"FOO": "1"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other"}]}], "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": "mine"}]}]}}"#;
        write_file(&path, std::str::from_utf8(original).unwrap());
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        let settings = json_at(&path);
        assert_eq!(settings["model"], "x");
        assert_eq!(settings["env"]["FOO"], "1");
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["command"], "other");
        let session = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session.len(), 2);
        assert_eq!(session[0]["hooks"][0]["command"], "mine");
        assert_eq!(
            session[1]["hooks"][0]["command"],
            "/opt/cetra/bin/cetra agents hook claude-code"
        );
        assert_eq!(fs::read(backup_of(&path)).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let path = claude_path(&paths);
        write_file(&path, "{\"model\": \"x\"}\n");
        let original = fs::read(&path).unwrap();
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        let first = fs::read(&path).unwrap();
        let second_output = run_setup(&setup_args(Agent::ClaudeCode), &paths);
        assert_eq!(fs::read(&path).unwrap(), first);
        assert!(!second_output.contains("wrote"), "{second_output}");
        assert_eq!(
            json_at(&path)["hooks"]["SessionStart"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(fs::read(backup_of(&path)).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_no_content_removes_content_flags() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::ClaudeCode);
        args.no_content = true;
        run_setup(&args, &paths);
        let settings = json_at(&claude_path(&paths));
        let env = settings["env"].as_object().unwrap();
        assert_eq!(env.len(), 9);
        for name in CLAUDE_CONTENT {
            assert!(env.get(name).is_none(), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_rejects_invalid_json_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let path = claude_path(&paths);
        write_file(&path, "not json");
        let error = setup(&setup_args(Agent::ClaudeCode), &paths, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid JSON"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        assert!(!backup_of(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn claude_print_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::ClaudeCode);
        args.print = true;
        let out = run_setup(&args, &paths);
        assert!(
            out.contains(&format!("# {}", claude_path(&paths).display())),
            "{out}"
        );
        assert!(out.contains("\"OTEL_EXPORTER_OTLP_ENDPOINT\""), "{out}");
        assert!(!claude_path(&paths).exists());
        assert!(!paths.agents_file().exists());
    }

    #[cfg(unix)]
    #[test]
    fn claude_setup_follows_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let target = paths.home.join("dotfiles").join("claude.json");
        write_file(&target, "{\"model\": \"x\"}\n");
        let link = claude_path(&paths);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let settings = json_at(&target);
        assert_eq!(settings["model"], "x");
        assert!(settings["env"].is_object());
    }

    #[cfg(unix)]
    #[test]
    fn claude_remove_restores_the_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let path = claude_path(&paths);
        let original = json!({"model": "x", "env": {"FOO": "1"}});
        write_file(&path, &serde_json::to_string_pretty(&original).unwrap());
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        run_remove(Agent::ClaudeCode, &paths);
        assert_eq!(json_at(&path), original);
    }

    #[cfg(unix)]
    #[test]
    fn claude_remove_twice() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        let first = run_remove(Agent::ClaudeCode, &paths);
        assert!(first.contains("updated"), "{first}");
        let bytes = fs::read(claude_path(&paths)).unwrap();
        let second = run_remove(Agent::ClaudeCode, &paths);
        assert_eq!(second, "nothing to remove\n");
        assert_eq!(fs::read(claude_path(&paths)).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn claude_remove_leaves_env_pointing_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let path = claude_path(&paths);
        write_file(
            &path,
            "{\"env\": {\"OTEL_EXPORTER_OTLP_ENDPOINT\": \"https://other.example\", \"FOO\": \"1\"}}\n",
        );
        save_agents(&paths, "https://cetra.example.com");
        let out = run_remove(Agent::ClaudeCode, &paths);
        assert!(out.contains("another server"), "{out}");
        assert_eq!(
            json_at(&path)["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "https://other.example"
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_remove_without_agents_toml_removes_hooks_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&setup_args(Agent::ClaudeCode), &paths);
        fs::remove_file(paths.agents_file()).unwrap();
        let out = run_remove(Agent::ClaudeCode, &paths);
        assert!(out.contains("agents.toml not found"), "{out}");
        let settings = json_at(&claude_path(&paths));
        assert!(settings.get("hooks").is_none());
        assert_eq!(
            settings["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "https://cetra.example.com"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gemini_setup_writes_settings_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let gemini = gemini_dir(&paths);
        write_file(&gemini.join(".env"), "GEMINI_API_KEY=x\r\n");
        run_setup(&setup_args(Agent::GeminiCli), &paths);
        let env = fs::read_to_string(gemini.join(".env")).unwrap();
        assert!(env.starts_with("GEMINI_API_KEY=x\r\n"), "{env:?}");
        assert!(
            env.contains("\nOTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-cetra-test\n"),
            "{env:?}"
        );
        let settings = json_at(&gemini.join("settings.json"));
        assert_eq!(
            settings["telemetry"]["otlpEndpoint"],
            "https://cetra.example.com"
        );
        assert_eq!(settings["telemetry"]["logPrompts"], true);
        assert_eq!(
            settings["hooks"]["BeforeAgent"][0]["hooks"][0]["command"],
            "/opt/cetra/bin/cetra agents hook gemini-cli"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gemini_setup_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let gemini = gemini_dir(&paths);
        run_setup(&setup_args(Agent::GeminiCli), &paths);
        let settings = fs::read(gemini.join("settings.json")).unwrap();
        let env = fs::read(gemini.join(".env")).unwrap();
        let output = run_setup(&setup_args(Agent::GeminiCli), &paths);
        assert_eq!(fs::read(gemini.join("settings.json")).unwrap(), settings);
        assert_eq!(fs::read(gemini.join(".env")).unwrap(), env);
        assert!(!output.contains("wrote"), "{output}");
    }

    #[cfg(unix)]
    #[test]
    fn gemini_remove_restores() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let gemini = gemini_dir(&paths);
        write_file(&gemini.join("settings.json"), "{\"theme\": \"dark\"}\n");
        run_setup(&setup_args(Agent::GeminiCli), &paths);
        run_remove(Agent::GeminiCli, &paths);
        assert_eq!(
            json_at(&gemini.join("settings.json")),
            json!({"theme": "dark"})
        );
        let env = fs::read_to_string(gemini.join(".env")).unwrap();
        assert!(!env.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{env:?}");
    }

    #[cfg(unix)]
    #[test]
    fn gemini_no_content_sets_log_prompts_false() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::GeminiCli);
        args.no_content = true;
        run_setup(&args, &paths);
        let settings = json_at(&gemini_dir(&paths).join("settings.json"));
        assert_eq!(settings["telemetry"]["logPrompts"], false);
    }

    #[test]
    fn copilot_prints_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let out = run_setup(&setup_args(Agent::Copilot), &paths);
        assert!(
            out.contains("export OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com\n"),
            "{out}"
        );
        assert!(out.contains("export OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY"));
        assert!(!paths.agents_file().exists());
        assert!(!paths.home.exists());
        let removed = run_remove(Agent::Copilot, &paths);
        assert!(removed.contains("nothing to remove"), "{removed}");
    }

    #[test]
    fn copilot_no_content() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::Copilot);
        args.no_content = true;
        let out = run_setup(&args, &paths);
        assert!(!out.contains("CAPTURE_CONTENT"), "{out}");
        assert!(
            out.contains("\"github.copilot.chat.otel.captureContent\": false"),
            "{out}"
        );
    }

    #[test]
    fn copilot_http_note() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::Copilot);
        args.url = "http://cetra.test:8080".to_owned();
        let out = run_setup(&args, &paths);
        assert!(
            out.contains("Copilot CLI does not export to http:// endpoints"),
            "{out}"
        );
    }

    #[test]
    fn setup_refuses_a_non_sk_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::ClaudeCode);
        args.key = Token::from_str("pat-secret-value").unwrap();
        let error = setup(&args, &paths, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("sk-cetra-"), "{error}");
        assert!(!error.contains("pat-secret-value"), "{error}");
    }

    #[test]
    fn setup_refuses_a_bad_url() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = setup_args(Agent::ClaudeCode);
        args.url = "ftp://cetra.example.com".to_owned();
        let error = setup(&args, &paths, &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert_eq!(error, "--url must start with http:// or https://");
    }
}
