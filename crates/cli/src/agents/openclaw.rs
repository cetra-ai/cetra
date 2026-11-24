//! OpenClaw (D-IN-28 items 5 and 6). Setup installs OpenClaw's `diagnostics-otel` plugin at the
//! version `openclaw --version` prints, then merges `~/.openclaw/openclaw.json`. Remove undoes the
//! merge and leaves the plugin installed (it prints the uninstall command instead).

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::hook::run_with_timeout;
use super::setup::{
    NOT_FOUND_NOTE, OTHER_SERVER_NOTE, emit, object_entry, points_at, read_json_object,
    settings_bytes, updated,
};
use super::{AgentsFile, Paths, SetupArgs};

const PLUGIN: &str = "diagnostics-otel";
const PACKAGE: &str = "@openclaw/diagnostics-otel";
/// The `diagnostics.otel` keys setup sets; remove takes them back.
const OTEL_KEYS: [&str; 9] = [
    "enabled",
    "endpoint",
    "protocol",
    "serviceName",
    "traces",
    "metrics",
    "logs",
    "captureContent",
    "headers",
];
const RESTART_NOTE: &str = "Restart the OpenClaw gateway if one is running (openclaw gateway restart). One-shot runs (openclaw agent --local) pick it up now.";
const UNINSTALL_NOTE: &str =
    "To remove the plugin too, run: openclaw plugins uninstall diagnostics-otel";

fn openclaw_dir(paths: &Paths) -> PathBuf {
    paths.home.join(".openclaw")
}

/// The OpenClaw version in `openclaw --version`'s output: the first whitespace-separated token
/// of the form `YYYY.M.D` (`OpenClaw 2026.11.5 (e13cf15)` gives `2026.11.5`).
pub(super) fn parse_openclaw_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| is_version(token))
        .map(str::to_owned)
}

/// `^\d{4}\.\d{1,2}\.\d{1,2}$`, checked without a regex.
fn is_version(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    matches!(parts.as_slice(), [year, month, day]
        if year.len() == 4
            && all_digits(year)
            && (1..=2).contains(&month.len())
            && all_digits(month)
            && (1..=2).contains(&day.len())
            && all_digits(day))
}

fn all_digits(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The version from the captured output of `openclaw --version` (`None` when the program did
/// not run or exited non-zero).
fn version_from_output(output: Option<String>) -> anyhow::Result<String> {
    let output =
        output.ok_or_else(|| anyhow::anyhow!("openclaw is not installed or not on PATH"))?;
    parse_openclaw_version(&output).ok_or_else(|| {
        anyhow::anyhow!("could not read the OpenClaw version from `openclaw --version`")
    })
}

/// The real version call: `openclaw --version` on PATH, within 10 seconds.
pub(super) fn installed_version() -> anyhow::Result<String> {
    version_from_output(run_with_timeout(
        "openclaw",
        &["--version"],
        &[],
        Duration::from_secs(10),
    ))
}

/// The real install: `openclaw` with `args`, inheriting stdout and stderr, no timeout. A
/// non-zero exit is an error.
pub(super) fn run_install(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("openclaw")
        .args(args)
        .status()
        .context("starting openclaw")?;
    anyhow::ensure!(status.success(), "openclaw exited with {status}");
    Ok(())
}

/// The `diagnostics.otel` object, when present.
fn otel_section(root: &Map<String, Value>) -> Option<&Value> {
    root.get("diagnostics")
        .and_then(|diagnostics| diagnostics.get("otel"))
}

/// Sets the plugin's allow-list entry, its enabled entry, `diagnostics.enabled` and each
/// `diagnostics.otel` key setup owns. Other entries and keys keep their place.
fn merge(root: &mut Map<String, Value>, url: &str, args: &SetupArgs) -> anyhow::Result<()> {
    let plugins = object_entry(root, "plugins")?;
    let allow = plugins
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("the plugins.allow setting is not an array")?;
    if !allow.iter().any(|name| name.as_str() == Some(PLUGIN)) {
        allow.push(json!(PLUGIN));
    }
    let entries = object_entry(plugins, "entries")?;
    object_entry(entries, PLUGIN)?.insert("enabled".to_owned(), Value::Bool(true));

    let diagnostics = object_entry(root, "diagnostics")?;
    diagnostics.insert("enabled".to_owned(), Value::Bool(true));
    let otel = object_entry(diagnostics, "otel")?;
    let values = [
        ("enabled", Value::Bool(true)),
        ("endpoint", json!(url)),
        ("protocol", json!("http/protobuf")),
        ("serviceName", json!("openclaw")),
        ("traces", Value::Bool(true)),
        ("metrics", Value::Bool(false)),
        ("logs", Value::Bool(false)),
        ("captureContent", Value::Bool(!args.no_content)),
        (
            "headers",
            json!({"Authorization": format!("Bearer {}", args.key.expose())}),
        ),
    ];
    for (name, value) in values {
        otel.insert(name.to_owned(), value);
    }
    Ok(())
}

/// Removes `map[key]` when it is an empty object or array.
fn prune_empty(map: &mut Map<String, Value>, key: &str) {
    let empty = match map.get(key) {
        Some(Value::Object(object)) => object.is_empty(),
        Some(Value::Array(array)) => array.is_empty(),
        _ => false,
    };
    if empty {
        map.remove(key);
    }
}

/// Undoes `merge`: the `diagnostics.otel` keys it set, the plugin's allow-list entry and its
/// `entries` object. `diagnostics.enabled` stays, since it may predate setup.
fn unmerge(root: &mut Map<String, Value>) {
    if let Some(diagnostics) = root.get_mut("diagnostics").and_then(Value::as_object_mut) {
        if let Some(otel) = diagnostics.get_mut("otel").and_then(Value::as_object_mut) {
            for key in OTEL_KEYS {
                otel.remove(key);
            }
        }
        prune_empty(diagnostics, "otel");
    }
    if let Some(plugins) = root.get_mut("plugins").and_then(Value::as_object_mut) {
        if let Some(allow) = plugins.get_mut("allow").and_then(Value::as_array_mut) {
            allow.retain(|name| name.as_str() != Some(PLUGIN));
        }
        if let Some(entries) = plugins.get_mut("entries").and_then(Value::as_object_mut) {
            entries.remove(PLUGIN);
        }
        prune_empty(plugins, "entries");
        prune_empty(plugins, "allow");
        prune_empty(root, "plugins");
    }
}

/// `cetra agents setup openclaw`. `version` reads OpenClaw's version and `install` runs the
/// plugin install with the arguments after `openclaw`; the real ones are `installed_version`
/// and `run_install`. Results and notes go to `out`.
pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
    version: &dyn Fn() -> anyhow::Result<String>,
    install: &dyn Fn(&[&str]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let version = version()?;
    let spec = format!("npm:{PACKAGE}@{version}");
    let install_args = [
        "plugins",
        "install",
        "--pin",
        "--accept-capabilities",
        spec.as_str(),
    ];
    // The config is merged before the install runs, so an unreadable config fails before the
    // plugin is installed. Nothing is written until the install has succeeded.
    let path = openclaw_dir(paths).join("openclaw.json");
    let mut root = read_json_object(&path)?;
    merge(&mut root, url, args)?;
    if args.print {
        writeln!(out, "openclaw {}", install_args.join(" "))?;
    } else {
        install(&install_args)
            .map_err(|_| anyhow::anyhow!("the plugin install failed; nothing was written"))?;
    }
    emit(&path, &settings_bytes(&root)?, args.print, out)?;
    if !args.print {
        writeln!(out, "{RESTART_NOTE}")?;
    }
    Ok(())
}

/// `cetra agents remove openclaw`. Undoes the config merge when `diagnostics.otel.endpoint`
/// is the saved server. Results and notes go to `out`.
pub(super) fn remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    let agents = AgentsFile::load(&paths.agents_file())?;
    let path = openclaw_dir(paths).join("openclaw.json");
    let mut root = read_json_object(&path)?;
    let original = root.clone();
    match &agents {
        None => writeln!(out, "{NOT_FOUND_NOTE}")?,
        Some(file) => {
            if points_at(otel_section(&root), "endpoint", &file.url) {
                unmerge(&mut root);
            } else if otel_section(&root)
                .and_then(|otel| otel.get("endpoint"))
                .is_some()
            {
                writeln!(out, "{OTHER_SERVER_NOTE}")?;
            }
        }
    }
    let mut changed = false;
    if root != original {
        changed |= updated(&path, &settings_bytes(&root)?, out)?;
    }
    if !changed {
        writeln!(out, "nothing to remove")?;
    }
    writeln!(out, "{UNINSTALL_NOTE}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;
    use crate::agents::Agent;
    use crate::client::Token;

    const URL: &str = "https://cetra.example.com";
    const VERSION: &str = "2026.11.5";
    /// `openclaw --version`'s output as recorded in the M7 spike (`spikes/m7-openclaw-omnigent.md`
    /// § 4 and the plan's IN7.4 step 2).
    const RECORDED_VERSION_OUTPUT: &str = "OpenClaw 2026.11.5 (e13cf15)\n";

    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            cetra_dir: root.join("cetra"),
            exe: PathBuf::from("/opt/cetra/bin/cetra"),
        }
    }

    fn openclaw_args(no_content: bool, print: bool) -> SetupArgs {
        SetupArgs {
            agent: Agent::OpenClaw,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: None,
            no_content,
            print,
        }
    }

    /// Runs setup with a version of `VERSION` and an install that succeeds unless `install_ok`
    /// is false. Returns the result, the output and the arguments of each install call.
    fn run_setup(
        args: &SetupArgs,
        paths: &Paths,
        install_ok: bool,
    ) -> (anyhow::Result<String>, Vec<Vec<String>>) {
        let calls = RefCell::new(Vec::new());
        let mut out = Vec::new();
        let result = setup(
            args,
            URL,
            paths,
            &mut out,
            &|| Ok(VERSION.to_owned()),
            &|install_args: &[&str]| {
                calls
                    .borrow_mut()
                    .push(install_args.iter().map(|arg| (*arg).to_owned()).collect());
                if install_ok {
                    Ok(())
                } else {
                    anyhow::bail!("exit status 1")
                }
            },
        )
        .map(|()| String::from_utf8(out).unwrap());
        (result, calls.into_inner())
    }

    fn run_remove(paths: &Paths) -> String {
        let mut out = Vec::new();
        remove(paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn config_path(paths: &Paths) -> PathBuf {
        openclaw_dir(paths).join("openclaw.json")
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
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
    fn openclaw_setup_installs_the_matching_plugin_then_merges() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let (result, calls) = run_setup(&openclaw_args(false, false), &paths, true);
        result.unwrap();
        assert_eq!(
            calls,
            vec![vec![
                "plugins".to_owned(),
                "install".to_owned(),
                "--pin".to_owned(),
                "--accept-capabilities".to_owned(),
                "npm:@openclaw/diagnostics-otel@2026.11.5".to_owned(),
            ]]
        );
        let config = json_at(&config_path(&paths));
        assert_eq!(config["plugins"]["allow"], json!(["diagnostics-otel"]));
        assert_eq!(
            config["plugins"]["entries"]["diagnostics-otel"]["enabled"],
            true
        );
        assert_eq!(config["diagnostics"]["enabled"], true);
        let otel = &config["diagnostics"]["otel"];
        assert_eq!(otel["enabled"], true);
        assert_eq!(otel["endpoint"], URL);
        assert_eq!(otel["protocol"], "http/protobuf");
        assert_eq!(otel["serviceName"], "openclaw");
        assert_eq!(otel["traces"], true);
        assert_eq!(otel["metrics"], false);
        assert_eq!(otel["logs"], false);
        assert_eq!(otel["captureContent"], true);
        assert_eq!(
            otel["headers"],
            json!({"Authorization": "Bearer sk-cetra-test"})
        );
    }

    #[test]
    fn openclaw_setup_keeps_other_plugins_and_keys() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        write_file(
            &config_path(&paths),
            r#"{
  "model": "x",
  "plugins": {"allow": ["other"], "entries": {"other": {"enabled": true}}},
  "diagnostics": {"enabled": false, "otel": {"customKey": 1, "endpoint": "https://old.example.com"}}
}
"#,
        );
        run_setup(&openclaw_args(false, false), &paths, true)
            .0
            .unwrap();
        let config = json_at(&config_path(&paths));
        assert_eq!(config["model"], "x");
        assert_eq!(
            config["plugins"]["allow"],
            json!(["other", "diagnostics-otel"])
        );
        assert_eq!(
            config["plugins"]["entries"]["other"],
            json!({"enabled": true})
        );
        assert_eq!(
            config["plugins"]["entries"]["diagnostics-otel"]["enabled"],
            true
        );
        assert_eq!(config["diagnostics"]["enabled"], true);
        assert_eq!(config["diagnostics"]["otel"]["customKey"], 1);
        assert_eq!(config["diagnostics"]["otel"]["endpoint"], URL);
    }

    #[test]
    fn openclaw_setup_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&openclaw_args(false, false), &paths, true)
            .0
            .unwrap();
        let first = fs::read(config_path(&paths)).unwrap();
        let (result, _) = run_setup(&openclaw_args(false, false), &paths, true);
        let output = result.unwrap();
        assert_eq!(fs::read(config_path(&paths)).unwrap(), first);
        assert!(!output.contains("wrote"), "{output}");
        let config = json_at(&config_path(&paths));
        assert_eq!(config["plugins"]["allow"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn openclaw_install_failure_writes_nothing() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let (result, calls) = run_setup(&openclaw_args(false, false), &paths, false);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            result.unwrap_err().to_string(),
            "the plugin install failed; nothing was written"
        );
        assert!(!config_path(&paths).exists());
        assert!(!openclaw_dir(&paths).exists());
    }

    #[test]
    fn openclaw_print_runs_nothing_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let (result, calls) = run_setup(&openclaw_args(false, true), &paths, true);
        let output = result.unwrap();
        assert!(calls.is_empty(), "{calls:?}");
        assert!(!config_path(&paths).exists());
        assert!(
            output.contains(
                "openclaw plugins install --pin --accept-capabilities npm:@openclaw/diagnostics-otel@2026.11.5"
            ),
            "{output}"
        );
        assert!(output.contains("sk-cetra-test"), "{output}");
    }

    #[test]
    fn openclaw_no_content_sets_capture_false() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&openclaw_args(true, false), &paths, true)
            .0
            .unwrap();
        let config = json_at(&config_path(&paths));
        assert_eq!(config["diagnostics"]["otel"]["captureContent"], false);
    }

    #[test]
    fn openclaw_version_parses_the_recorded_output() {
        assert_eq!(
            parse_openclaw_version(RECORDED_VERSION_OUTPUT).as_deref(),
            Some("2026.11.5")
        );
        assert_eq!(
            parse_openclaw_version("2026.1.12").as_deref(),
            Some("2026.1.12")
        );
        assert_eq!(parse_openclaw_version("OpenClaw 26.11.5"), None);
        assert_eq!(parse_openclaw_version("OpenClaw 2026.11"), None);
        assert_eq!(parse_openclaw_version("2026.111.5"), None);
        assert_eq!(parse_openclaw_version("no version here"), None);
        assert_eq!(
            version_from_output(Some("nothing useful".to_owned()))
                .unwrap_err()
                .to_string(),
            "could not read the OpenClaw version from `openclaw --version`"
        );
    }

    #[test]
    fn openclaw_missing_binary_message() {
        assert_eq!(
            version_from_output(None).unwrap_err().to_string(),
            "openclaw is not installed or not on PATH"
        );
    }

    #[test]
    fn openclaw_remove_undoes_the_merge() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        write_file(
            &config_path(&paths),
            "{\"theme\": \"dark\", \"plugins\": {\"allow\": [\"other\"]}}\n",
        );
        save_agents(&paths, URL);
        run_setup(&openclaw_args(false, false), &paths, true)
            .0
            .unwrap();
        let out = run_remove(&paths);
        assert!(out.contains("updated"), "{out}");
        assert!(
            out.contains("openclaw plugins uninstall diagnostics-otel"),
            "{out}"
        );
        assert_eq!(
            json_at(&config_path(&paths)),
            json!({
                "theme": "dark",
                "plugins": {"allow": ["other"]},
                "diagnostics": {"enabled": true}
            })
        );
    }

    #[test]
    fn openclaw_remove_leaves_another_endpoint() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&openclaw_args(false, false), &paths, true)
            .0
            .unwrap();
        let before = fs::read(config_path(&paths)).unwrap();
        save_agents(&paths, "https://other.example.com");
        let out = run_remove(&paths);
        assert!(out.contains(OTHER_SERVER_NOTE), "{out}");
        assert!(out.contains("nothing to remove"), "{out}");
        assert_eq!(fs::read(config_path(&paths)).unwrap(), before);
    }
}
