//! Codex (D-CA-10): `<codex home>/config.toml` (its `[otel]` table, edited in place so comments
//! and key order stay) and `<codex home>/hooks.json`. The codex home is `$CODEX_HOME` or
//! `~/.codex`; the caller passes it in, so tests never set the variable.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, table, value};

use super::setup::{
    NOT_FOUND_NOTE, OTHER_SERVER_NOTE, emit, hook_command, read_json_object, read_optional_text,
    remove_hooks, set_hooks, settings_bytes, updated,
};
use super::{Agent, AgentsFile, Paths, SetupArgs};

const CODEX_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];

/// `$CODEX_HOME` when set and not empty, else `<home>/.codex`.
pub(super) fn codex_home(paths: &Paths) -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => paths.home.join(".codex"),
    }
}

pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    codex_home: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config_path = codex_home.join("config.toml");
    let text = read_optional_text(&config_path)?.unwrap_or_default();
    let mut doc = parse_config(&text, &config_path)?;
    let otel = otel_table(&mut doc, &config_path)?;
    otel.insert("exporter", value("none"));
    otel.insert("metrics_exporter", value("none"));
    otel.insert(
        "trace_exporter",
        Item::Value(Value::InlineTable(trace_exporter(url, args.key.expose()))),
    );
    emit(&config_path, doc.to_string().as_bytes(), args.print, out)?;

    let hooks_path = codex_home.join("hooks.json");
    let mut root = read_json_object(&hooks_path)?;
    set_hooks(
        &mut root,
        &CODEX_EVENTS,
        &hook_command(paths, Agent::Codex),
        Agent::Codex,
    )?;
    emit(&hooks_path, &settings_bytes(&root)?, args.print, out)?;
    if !args.print {
        writeln!(
            out,
            "Codex runs the cetra hook only after you trust it in Codex; until then, traces have no repository or branch."
        )?;
    }
    Ok(())
}

pub(super) fn remove(paths: &Paths, codex_home: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let agents = AgentsFile::load(&paths.agents_file())?;
    let config_path = codex_home.join("config.toml");
    let hooks_path = codex_home.join("hooks.json");
    let text = read_optional_text(&config_path)?.unwrap_or_default();
    let mut doc = parse_config(&text, &config_path)?;
    match &agents {
        None => writeln!(out, "{NOT_FOUND_NOTE}")?,
        Some(file) => {
            let endpoint = format!("{}/v1/traces", file.url);
            if trace_endpoint(&doc) == Some(endpoint.as_str()) {
                remove_our_exporter(&mut doc);
            } else if trace_endpoint(&doc).is_some() {
                writeln!(out, "{OTHER_SERVER_NOTE}")?;
            }
        }
    }
    let config = doc.to_string();
    let mut changed = false;
    if config != text {
        changed |= updated(&config_path, config.as_bytes(), out)?;
    }

    let mut root = read_json_object(&hooks_path)?;
    let original = root.clone();
    remove_hooks(&mut root, Agent::Codex)?;
    if root != original {
        changed |= updated(&hooks_path, &settings_bytes(&root)?, out)?;
    }
    if !changed {
        writeln!(out, "nothing to remove")?;
    }
    Ok(())
}

fn parse_config(text: &str, path: &Path) -> anyhow::Result<DocumentMut> {
    text.parse::<DocumentMut>().map_err(|_| {
        anyhow::anyhow!(
            "{} is not valid TOML; fix it or run with --print",
            path.display()
        )
    })
}

/// The `[otel]` table, created when absent.
fn otel_table<'a>(doc: &'a mut DocumentMut, path: &Path) -> anyhow::Result<&'a mut Table> {
    doc.as_table_mut()
        .entry("otel")
        .or_insert(table())
        .as_table_mut()
        .with_context(|| format!("{} has an otel setting that is not a table", path.display()))
}

/// `trace_exporter = { otlp-http = { endpoint, protocol = "binary", headers = { authorization } } }`.
fn trace_exporter(url: &str, key: &str) -> InlineTable {
    let mut headers = InlineTable::new();
    headers.insert("authorization", format!("Bearer {key}").into());
    let mut otlp = InlineTable::new();
    otlp.insert("endpoint", format!("{url}/v1/traces").into());
    otlp.insert("protocol", "binary".into());
    otlp.insert("headers", Value::InlineTable(headers));
    let mut exporter = InlineTable::new();
    exporter.insert("otlp-http", Value::InlineTable(otlp));
    exporter
}

/// The endpoint of `[otel] trace_exporter`'s `otlp-http`, when there is one.
fn trace_endpoint(doc: &DocumentMut) -> Option<&str> {
    doc.as_table()
        .get("otel")?
        .as_table_like()?
        .get("trace_exporter")?
        .as_table_like()?
        .get("otlp-http")?
        .as_table_like()?
        .get("endpoint")?
        .as_str()
}

/// Removes the trace exporter, then `exporter` and `metrics_exporter` when they are still
/// `"none"` (a value setup replaced is not restored), then `[otel]` when it is empty.
fn remove_our_exporter(doc: &mut DocumentMut) {
    let top = doc.as_table_mut();
    let Some(otel) = top.get_mut("otel").and_then(Item::as_table_mut) else {
        return;
    };
    otel.remove("trace_exporter");
    for key in ["exporter", "metrics_exporter"] {
        if otel.get(key).and_then(Item::as_str) == Some("none") {
            otel.remove(key);
        }
    }
    if otel.is_empty() {
        top.remove("otel");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;

    use super::*;
    use crate::client::Token;

    const URL: &str = "https://cetra.example.com";

    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            cetra_dir: root.join("cetra"),
            exe: PathBuf::from("/opt/cetra/bin/cetra"),
        }
    }

    fn codex_args() -> SetupArgs {
        SetupArgs {
            agent: Agent::Codex,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: Some("dev@example.com".to_owned()),
            no_content: false,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, paths: &Paths, home: &Path) -> String {
        let mut out = Vec::new();
        setup(args, URL, paths, home, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove(paths: &Paths, home: &Path) -> String {
        let mut out = Vec::new();
        remove(paths, home, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn save_agents(paths: &Paths) {
        AgentsFile {
            url: URL.to_owned(),
            key: "sk-cetra-test".to_owned(),
            user: None,
        }
        .save(&paths.agents_file())
        .unwrap();
    }

    #[test]
    fn codex_setup_keeps_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let home = dir.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        let original = "# my config\nmodel = \"x\"\n\n[profiles.a]\nkey = 1\n";
        fs::write(home.join("config.toml"), original).unwrap();
        run_setup(&codex_args(), &paths, &home);
        let text = fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(text.starts_with(original), "{text}");
        let doc = text.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["otel"]["exporter"].as_str(), Some("none"));
        assert_eq!(doc["otel"]["metrics_exporter"].as_str(), Some("none"));
        assert_eq!(
            doc["otel"]["trace_exporter"]["otlp-http"]["endpoint"].as_str(),
            Some("https://cetra.example.com/v1/traces")
        );
        assert_eq!(
            doc["otel"]["trace_exporter"]["otlp-http"]["headers"]["authorization"].as_str(),
            Some("Bearer sk-cetra-test")
        );
    }

    #[test]
    fn codex_setup_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let home = dir.path().join("codex");
        run_setup(&codex_args(), &paths, &home);
        let config = fs::read(home.join("config.toml")).unwrap();
        let hooks = fs::read(home.join("hooks.json")).unwrap();
        let output = run_setup(&codex_args(), &paths, &home);
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), hooks);
        assert!(!output.contains("wrote"), "{output}");
    }

    #[test]
    fn codex_remove_restores_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let home = dir.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        let original = "model = \"x\"\n";
        fs::write(home.join("config.toml"), original).unwrap();
        run_setup(&codex_args(), &paths, &home);
        save_agents(&paths);
        run_remove(&paths, &home);
        assert_eq!(
            fs::read_to_string(home.join("config.toml")).unwrap(),
            original
        );
    }

    #[test]
    fn codex_hooks_json_has_two_events() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let home = dir.path().join("codex");
        run_setup(&codex_args(), &paths, &home);
        let hooks: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("hooks.json")).unwrap()).unwrap();
        let events = hooks["hooks"].as_object().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.contains_key("SessionStart"));
        assert!(events.contains_key("UserPromptSubmit"));
    }

    #[test]
    fn codex_home_parameter_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let custom = dir.path().join("custom");
        run_setup(&codex_args(), &paths, &custom);
        assert!(custom.join("config.toml").is_file());
        assert!(custom.join("hooks.json").is_file());
        assert!(!paths.home.join(".codex").exists());
    }
}
