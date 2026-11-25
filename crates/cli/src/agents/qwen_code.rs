//! Qwen Code (D-IN-28 item 7), a Gemini CLI fork. Setup writes its `telemetry` keys and hooks to
//! `~/.qwen/settings.json` and the headers line to `~/.qwen/.env`, through the pair Gemini CLI
//! also uses (`settings_env_setup` and `settings_env_remove` in `setup.rs`).

use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::setup::{SettingsAgent, settings_env_remove, settings_env_setup};
use super::{Agent, Paths, SetupArgs};

/// The `telemetry` keys of Qwen Code's `settings.json` that setup sets. Qwen has no `traces`
/// key (D-IN-28 item 7).
const QWEN_TELEMETRY: [&str; 6] = [
    "enabled",
    "target",
    "otlpEndpoint",
    "otlpProtocol",
    "logPrompts",
    "includeSensitiveSpanAttributes",
];
/// Qwen 0.28.0 fired these in the spike; `BeforeAgent` did not fire (D-IN-28 item 7).
const QWEN_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];
const QWEN_NOTE: &str = "Qwen Code also sends logs and metrics; cetra accepts and drops them.";

fn qwen_dir(paths: &Paths) -> PathBuf {
    paths.home.join(".qwen")
}

/// `cetra agents setup qwen-code`. Results and notes go to `out`.
pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let spec = SettingsAgent {
        agent: Agent::QwenCode,
        dir: qwen_dir(paths),
        telemetry: vec![
            ("enabled", Value::Bool(true)),
            ("target", json!("local")),
            ("otlpEndpoint", json!(url)),
            ("otlpProtocol", json!("http")),
            ("logPrompts", Value::Bool(!args.no_content)),
            (
                "includeSensitiveSpanAttributes",
                Value::Bool(!args.no_content),
            ),
        ],
        removable: &QWEN_TELEMETRY,
        events: &QWEN_EVENTS,
        note: QWEN_NOTE,
    };
    settings_env_setup(&spec, args, paths, out)
}

/// `cetra agents remove qwen-code`. Results and notes go to `out`.
pub(super) fn remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    let spec = SettingsAgent {
        agent: Agent::QwenCode,
        dir: qwen_dir(paths),
        telemetry: Vec::new(),
        removable: &QWEN_TELEMETRY,
        events: &QWEN_EVENTS,
        note: "",
    };
    settings_env_remove(&spec, paths, out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::agents::AgentsFile;
    use crate::agents::setup::{remove as agents_remove, setup as agents_setup};
    use crate::client::Token;

    const URL: &str = "https://cetra.example.com";

    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            cetra_dir: root.join("cetra"),
            exe: PathBuf::from("/opt/cetra/bin/cetra"),
        }
    }

    fn qwen_args(no_content: bool) -> SetupArgs {
        SetupArgs {
            agent: Agent::QwenCode,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: Some("dev@example.com".to_owned()),
            no_content,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, paths: &Paths) -> String {
        let mut out = Vec::new();
        agents_setup(args, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove(paths: &Paths) -> String {
        let mut out = Vec::new();
        agents_remove(Agent::QwenCode, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn write_file(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn qwen_setup_writes_settings_hooks_and_env() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let qwen = qwen_dir(&paths);
        let out = run_setup(&qwen_args(false), &paths);

        let settings = json_at(&qwen.join("settings.json"));
        let telemetry = settings["telemetry"].as_object().unwrap();
        assert_eq!(telemetry.len(), 6, "{telemetry:?}");
        assert_eq!(telemetry["enabled"], true);
        assert_eq!(telemetry["target"], "local");
        assert_eq!(telemetry["otlpEndpoint"], URL);
        assert_eq!(telemetry["otlpProtocol"], "http");
        assert_eq!(telemetry["logPrompts"], true);
        assert_eq!(telemetry["includeSensitiveSpanAttributes"], true);
        assert!(telemetry.get("traces").is_none());

        assert_eq!(settings["hooks"].as_object().unwrap().len(), 2);
        for event in QWEN_EVENTS {
            assert_eq!(
                settings["hooks"][event][0]["hooks"][0]["command"],
                "/opt/cetra/bin/cetra agents hook qwen-code",
                "{event}"
            );
        }

        let env = fs::read_to_string(qwen.join(".env")).unwrap();
        assert_eq!(
            env,
            "OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-cetra-test\n"
        );
        assert!(
            AgentsFile::load(&paths.agents_file()).unwrap().is_some(),
            "agents.toml is saved"
        );
        assert!(
            out.contains("Qwen Code also sends logs and metrics; cetra accepts and drops them."),
            "{out}"
        );
    }

    #[test]
    fn qwen_setup_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let qwen = qwen_dir(&paths);
        run_setup(&qwen_args(false), &paths);
        let settings = fs::read(qwen.join("settings.json")).unwrap();
        let env = fs::read(qwen.join(".env")).unwrap();
        let output = run_setup(&qwen_args(false), &paths);
        assert_eq!(fs::read(qwen.join("settings.json")).unwrap(), settings);
        assert_eq!(fs::read(qwen.join(".env")).unwrap(), env);
        assert!(!output.contains("wrote"), "{output}");
    }

    #[test]
    fn qwen_no_content_turns_content_off() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&qwen_args(true), &paths);
        let settings = json_at(&qwen_dir(&paths).join("settings.json"));
        assert_eq!(settings["telemetry"]["logPrompts"], false);
        assert_eq!(
            settings["telemetry"]["includeSensitiveSpanAttributes"],
            false
        );
    }

    #[test]
    fn qwen_remove_restores() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let qwen = qwen_dir(&paths);
        write_file(&qwen.join("settings.json"), "{\"theme\": \"dark\"}\n");
        write_file(&qwen.join(".env"), "FOO=1\n");
        run_setup(&qwen_args(false), &paths);
        let out = run_remove(&paths);
        assert!(out.contains("updated"), "{out}");
        assert_eq!(
            json_at(&qwen.join("settings.json")),
            json!({"theme": "dark"})
        );
        assert_eq!(fs::read_to_string(qwen.join(".env")).unwrap(), "FOO=1\n");
    }

    #[test]
    fn qwen_hook_timeout_unit() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&qwen_args(false), &paths);
        let settings = json_at(&qwen_dir(&paths).join("settings.json"));
        for event in QWEN_EVENTS {
            assert_eq!(
                settings["hooks"][event][0]["hooks"][0]["timeout"], 5,
                "{event} takes seconds"
            );
        }
    }
}
