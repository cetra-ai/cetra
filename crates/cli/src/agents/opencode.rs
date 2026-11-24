//! OpenCode (D-IN-29 items 2, 3 and 4): `setup` writes cetra's OpenCode plugin,
//! `opencode_plugin.ts`, to OpenCode's global plugin folder and the config file the plugin
//! reads its endpoint, headers and content switch from. `remove` deletes both when cetra
//! wrote them. OpenCode has no hook and no `agents.toml`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Map, Value};

use super::setup::{emit_owned, read_optional_text, settings_bytes};
use super::{Paths, SetupArgs};

const PLUGIN: &str = include_str!("opencode_plugin.ts");
/// The plugin's first line. Setup overwrites, and remove deletes, only a file that starts
/// with it (D-IN-29 item 3).
const MARKER: &str = "// Managed by cetra (cetra agents setup opencode). Delete this file or run: cetra agents remove opencode";
/// The config's `managed_by`: the value that marks the config as ours (D-IN-29 item 3).
const MANAGED_BY: &str = "cetra agents setup opencode";
const PLUGIN_FILE: &str = "cetra-opencode.ts";
const CONFIG_FILE: &str = "cetra-opencode.json";

/// `$XDG_CONFIG_HOME/opencode` when `XDG_CONFIG_HOME` is set and not empty, else
/// `<home>/.config/opencode` (D-IN-29 item 2). The plugin computes the same folder.
pub(super) fn opencode_dir(paths: &Paths) -> PathBuf {
    opencode_dir_from(std::env::var_os("XDG_CONFIG_HOME"), paths)
}

/// `opencode_dir` with the variable's value passed in, so tests never set it.
fn opencode_dir_from(value: Option<OsString>, paths: &Paths) -> PathBuf {
    match value {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("opencode"),
        _ => paths.home.join(".config").join("opencode"),
    }
}

fn plugin_path(dir: &Path) -> PathBuf {
    dir.join("plugins").join(PLUGIN_FILE)
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

/// Whether a config file's text is ours: a JSON object whose `managed_by` is `MANAGED_BY`
/// (D-IN-29 item 3).
fn config_text_is_ours(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    value
        .get("managed_by")
        .and_then(Value::as_str)
        .is_some_and(|managed_by| managed_by == MANAGED_BY)
}

/// The config file's bytes (D-IN-29 item 3): this exact object, keys in this order, two-space
/// indent, final newline. The header value is plain, not percent-encoded: it is JSON.
fn config_bytes(url: &str, key: &str, capture: bool) -> anyhow::Result<Vec<u8>> {
    let mut headers = Map::new();
    headers.insert(
        "Authorization".to_owned(),
        Value::String(format!("Bearer {key}")),
    );
    let mut root = Map::new();
    root.insert(
        "managed_by".to_owned(),
        Value::String(MANAGED_BY.to_owned()),
    );
    root.insert("endpoint".to_owned(), Value::String(url.to_owned()));
    root.insert("headers".to_owned(), Value::Object(headers));
    root.insert("capture_content".to_owned(), Value::Bool(capture));
    settings_bytes(&root)
}

/// `cetra agents setup opencode`. Results and notes go to `out`.
pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    setup_in(args, url, &opencode_dir(paths), out)
}

/// `setup` with OpenCode's folder passed in.
fn setup_in(args: &SetupArgs, url: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let plugin = plugin_path(dir);
    if read_optional_text(&plugin)?.is_some_and(|text| !text.starts_with(MARKER)) {
        anyhow::bail!(
            "{} exists and was not written by cetra; move it away and run setup again",
            plugin.display()
        );
    }
    let config = config_path(dir);
    if read_optional_text(&config)?.is_some_and(|text| !config_text_is_ours(&text)) {
        anyhow::bail!(
            "{} exists and was not written by cetra; move it away and run setup again",
            config.display()
        );
    }
    // The config first: the plugin beside it reads it at OpenCode's next start.
    let bytes = config_bytes(url, args.key.expose(), !args.no_content)?;
    emit_owned(&config, &bytes, args.print, out)?;
    emit_owned(&plugin, PLUGIN.as_bytes(), args.print, out)?;
    writeln!(
        out,
        "OpenCode loads the plugin at its next start. Both the npm build (1.x) and the v2 build are supported."
    )?;
    Ok(())
}

/// `cetra agents remove opencode`. Results and notes go to `out`.
pub(super) fn remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    remove_in(&opencode_dir(paths), out)
}

/// `remove` with OpenCode's folder passed in.
fn remove_in(dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let plugin = plugin_path(dir);
    match read_optional_text(&plugin)? {
        None => writeln!(
            out,
            "no cetra OpenCode plugin found at {}",
            plugin.display()
        )?,
        Some(text) if text.starts_with(MARKER) => {
            fs::remove_file(&plugin).with_context(|| format!("removing {}", plugin.display()))?;
            writeln!(out, "removed {}", plugin.display())?;
        }
        Some(_) => writeln!(
            out,
            "{} was not written by cetra; left in place",
            plugin.display()
        )?,
    }
    let config = config_path(dir);
    match read_optional_text(&config)? {
        None => writeln!(
            out,
            "no cetra OpenCode config found at {}",
            config.display()
        )?,
        Some(text) if config_text_is_ours(&text) => {
            fs::remove_file(&config).with_context(|| format!("removing {}", config.display()))?;
            writeln!(out, "removed {}", config.display())?;
        }
        Some(_) => writeln!(
            out,
            "{} was not written by cetra; left in place",
            config.display()
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::str::FromStr;

    use tempfile::tempdir;

    use super::*;
    use crate::agents::Agent;
    use crate::client::Token;

    const URL: &str = "https://cetra.example.com";

    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            cetra_dir: root.join("cetra"),
            exe: PathBuf::from("/opt/cetra/bin/cetra"),
        }
    }

    fn opencode_args() -> SetupArgs {
        SetupArgs {
            agent: Agent::OpenCode,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: None,
            no_content: false,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, dir: &Path) -> String {
        let mut out = Vec::new();
        setup_in(args, URL, dir, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove(dir: &Path) -> String {
        let mut out = Vec::new();
        remove_in(dir, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn write_foreign_plugin(dir: &Path) {
        let path = plugin_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "export default function () {}\n").unwrap();
    }

    fn write_foreign_config(dir: &Path) {
        let path = config_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "{\"managed_by\": \"someone else\"}\n").unwrap();
    }

    fn write_our_config(dir: &Path) {
        let path = config_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, config_bytes(URL, "sk-cetra-test", true).unwrap()).unwrap();
    }

    fn write_our_plugin(dir: &Path) {
        let path = plugin_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, PLUGIN).unwrap();
    }

    #[test]
    fn opencode_setup_writes_the_plugin_and_the_config() {
        let dir = tempdir().unwrap();
        run_setup(&opencode_args(), dir.path());
        assert_eq!(fs::read_to_string(plugin_path(dir.path())).unwrap(), PLUGIN);
        // D-IN-29 item 3's exact shape: the keys in order, two-space indent, final newline.
        assert_eq!(
            fs::read_to_string(config_path(dir.path())).unwrap(),
            r#"{
  "managed_by": "cetra agents setup opencode",
  "endpoint": "https://cetra.example.com",
  "headers": {
    "Authorization": "Bearer sk-cetra-test"
  },
  "capture_content": true
}
"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config_path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn opencode_setup_uses_xdg_config_home() {
        let root = tempdir().unwrap();
        let paths = paths_under(root.path());
        let custom = root.path().join("custom-config");
        assert_eq!(
            opencode_dir_from(Some(custom.clone().into_os_string()), &paths),
            custom.join("opencode")
        );
        assert_eq!(
            opencode_dir_from(Some(OsString::new()), &paths),
            paths.home.join(".config").join("opencode")
        );
        assert_eq!(
            opencode_dir_from(None, &paths),
            paths.home.join(".config").join("opencode")
        );
        run_setup(&opencode_args(), &custom.join("opencode"));
        assert!(plugin_path(&custom.join("opencode")).is_file());
        assert!(config_path(&custom.join("opencode")).is_file());
        assert!(!paths.home.join(".config").exists());
    }

    #[test]
    fn opencode_no_content_sets_capture_false() {
        let dir = tempdir().unwrap();
        let mut args = opencode_args();
        args.no_content = true;
        run_setup(&args, dir.path());
        let config: Value =
            serde_json::from_str(&fs::read_to_string(config_path(dir.path())).unwrap()).unwrap();
        assert_eq!(config.get("capture_content"), Some(&Value::Bool(false)));
    }

    #[test]
    fn opencode_setup_refuses_a_foreign_plugin() {
        let dir = tempdir().unwrap();
        write_foreign_plugin(dir.path());
        let foreign = fs::read_to_string(plugin_path(dir.path())).unwrap();
        let mut out = Vec::new();
        let error = setup_in(&opencode_args(), URL, dir.path(), &mut out).unwrap_err();
        assert!(error.to_string().contains("was not written by cetra"));
        assert_eq!(
            fs::read_to_string(plugin_path(dir.path())).unwrap(),
            foreign
        );
        assert!(!config_path(dir.path()).exists());
    }

    #[test]
    fn opencode_setup_refuses_a_foreign_config() {
        let dir = tempdir().unwrap();
        write_foreign_config(dir.path());
        let foreign = fs::read_to_string(config_path(dir.path())).unwrap();
        let mut out = Vec::new();
        let error = setup_in(&opencode_args(), URL, dir.path(), &mut out).unwrap_err();
        assert!(error.to_string().contains("was not written by cetra"));
        assert_eq!(
            fs::read_to_string(config_path(dir.path())).unwrap(),
            foreign
        );
        assert!(!plugin_path(dir.path()).exists());
    }

    #[test]
    fn opencode_setup_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let first = run_setup(&opencode_args(), dir.path());
        let plugin = fs::read(plugin_path(dir.path())).unwrap();
        let config = fs::read(config_path(dir.path())).unwrap();
        let second = run_setup(&opencode_args(), dir.path());
        assert_eq!(fs::read(plugin_path(dir.path())).unwrap(), plugin);
        assert_eq!(fs::read(config_path(dir.path())).unwrap(), config);
        assert!(first.contains("wrote "));
        assert!(!second.contains("wrote "));
    }

    #[test]
    fn opencode_print_writes_nothing() {
        let dir = tempdir().unwrap();
        let mut args = opencode_args();
        args.print = true;
        let out = run_setup(&args, dir.path());
        assert!(!plugin_path(dir.path()).exists());
        assert!(!config_path(dir.path()).exists());
        assert!(out.contains(MARKER));
    }

    #[test]
    fn opencode_remove_deletes_only_our_files() {
        let dir = tempdir().unwrap();
        run_setup(&opencode_args(), dir.path());
        let out = run_remove(dir.path());
        assert!(!plugin_path(dir.path()).exists());
        assert!(!config_path(dir.path()).exists());
        assert!(out.contains("removed "));

        // A foreign plugin stays; our config still goes.
        write_foreign_plugin(dir.path());
        write_our_config(dir.path());
        let out = run_remove(dir.path());
        assert!(plugin_path(dir.path()).exists());
        assert!(out.contains(&format!(
            "{} was not written by cetra; left in place",
            plugin_path(dir.path()).display()
        )));
        assert!(!config_path(dir.path()).exists());

        // An absent plugin is reported; our config still goes.
        fs::remove_file(plugin_path(dir.path())).unwrap();
        write_our_config(dir.path());
        let out = run_remove(dir.path());
        assert!(out.contains(&format!(
            "no cetra OpenCode plugin found at {}",
            plugin_path(dir.path()).display()
        )));
        assert!(!config_path(dir.path()).exists());

        // A foreign config stays; our plugin still goes.
        write_our_plugin(dir.path());
        write_foreign_config(dir.path());
        let out = run_remove(dir.path());
        assert!(!plugin_path(dir.path()).exists());
        assert!(config_path(dir.path()).exists());
        assert!(out.contains(&format!(
            "{} was not written by cetra; left in place",
            config_path(dir.path()).display()
        )));

        // An absent config is reported.
        fs::remove_file(config_path(dir.path())).unwrap();
        write_our_plugin(dir.path());
        let out = run_remove(dir.path());
        assert!(!plugin_path(dir.path()).exists());
        assert!(out.contains(&format!(
            "no cetra OpenCode config found at {}",
            config_path(dir.path()).display()
        )));
    }

    #[test]
    fn opencode_plugin_imports_only_node_fs() {
        let imports: Vec<&str> = PLUGIN
            .lines()
            .filter(|line| line.trim_start().starts_with("import "))
            .collect();
        assert_eq!(imports, ["import { readFileSync } from \"node:fs\";"]);
        assert!(!PLUGIN.contains("require("));
    }

    #[test]
    fn opencode_plugin_scope_is_the_detection_key() {
        assert!(PLUGIN.contains("\"cetra-opencode\""));
    }

    #[test]
    fn opencode_plugin_reads_no_otel_endpoint() {
        assert!(!PLUGIN.contains("OTEL_EXPORTER_OTLP"));
    }

    #[test]
    fn opencode_plugin_node_tests_pass() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipped: node is not on PATH");
            return;
        }
        let test_file = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agents/opencode_plugin.test.mjs"
        );
        let output = Command::new("node")
            .args(["--test", test_file])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "node --test failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
