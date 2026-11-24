//! Pi (D-IN-28 items 2, 3 and 9): `setup` writes cetra's pi extension, `pi_extension.ts`, to pi's
//! global extension folder and prints the environment lines pi reads its endpoint and key from.
//! `remove` deletes the extension when cetra wrote it. Pi has no hook and no `agents.toml`.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::setup::{emit_owned, read_optional_text, shell_quote};
use super::{Paths, SetupArgs};

const EXTENSION: &str = include_str!("pi_extension.ts");
/// The extension's first line. Setup overwrites, and remove deletes, only a file that starts
/// with it (D-IN-28 item 3).
const MARKER: &str =
    "// Managed by cetra (cetra agents setup pi). Delete this file or run: cetra agents remove pi";
const EXTENSION_FILE: &str = "cetra.ts";

/// `$PI_CODING_AGENT_DIR` when set and not empty, else `<home>/.pi/agent`.
pub(super) fn pi_dir(paths: &Paths) -> PathBuf {
    pi_dir_from(std::env::var_os("PI_CODING_AGENT_DIR"), paths)
}

/// `pi_dir` with the variable's value passed in, so tests never set it.
fn pi_dir_from(value: Option<OsString>, paths: &Paths) -> PathBuf {
    match value {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => paths.home.join(".pi").join("agent"),
    }
}

fn extension_path(dir: &Path) -> PathBuf {
    dir.join("extensions").join(EXTENSION_FILE)
}

/// `cetra agents setup pi`. Results and notes go to `out`.
pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    setup_in(args, url, &pi_dir(paths), out)
}

/// `setup` with pi's directory passed in.
fn setup_in(args: &SetupArgs, url: &str, dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let path = extension_path(dir);
    if read_optional_text(&path)?.is_some_and(|text| !text.starts_with(MARKER)) {
        anyhow::bail!(
            "{} exists and was not written by cetra; move it away and run setup again",
            path.display()
        );
    }
    emit_owned(&path, EXTENSION.as_bytes(), args.print, out)?;
    writeln!(
        out,
        "pi reads its endpoint and key from the environment. Add these lines to your shell profile:"
    )?;
    let headers = format!("Authorization=Bearer%20{}", args.key.expose());
    writeln!(
        out,
        "export OTEL_EXPORTER_OTLP_ENDPOINT={}",
        shell_quote(Path::new(url))
    )?;
    writeln!(
        out,
        "export OTEL_EXPORTER_OTLP_HEADERS={}",
        shell_quote(Path::new(&headers))
    )?;
    if args.no_content {
        writeln!(
            out,
            "unset OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT"
        )?;
        writeln!(
            out,
            "Remove the content line from your profile if an earlier setup added it."
        )?;
    } else {
        writeln!(
            out,
            "export OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true"
        )?;
    }
    Ok(())
}

/// `cetra agents remove pi`. Results and notes go to `out`.
pub(super) fn remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    remove_in(&pi_dir(paths), out)
}

/// `remove` with pi's directory passed in.
fn remove_in(dir: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let path = extension_path(dir);
    match read_optional_text(&path)? {
        None => writeln!(out, "no cetra pi extension found at {}", path.display())?,
        Some(text) if text.starts_with(MARKER) => {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            writeln!(out, "removed {}", path.display())?;
        }
        Some(_) => writeln!(
            out,
            "{} was not written by cetra; left in place",
            path.display()
        )?,
    }
    writeln!(
        out,
        "Delete the OTEL_* lines that cetra agents setup pi printed from your shell profile."
    )?;
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

    fn pi_args() -> SetupArgs {
        SetupArgs {
            agent: Agent::Pi,
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

    fn write_foreign(dir: &Path) {
        let path = extension_path(dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "export default function () {}\n").unwrap();
    }

    #[test]
    fn pi_setup_writes_the_extension_with_the_marker() {
        let dir = tempdir().unwrap();
        run_setup(&pi_args(), dir.path());
        let written = fs::read_to_string(extension_path(dir.path())).unwrap();
        assert_eq!(written, EXTENSION);
        assert!(written.starts_with(MARKER));
        assert_eq!(written.lines().next(), Some(MARKER));
    }

    #[test]
    fn pi_setup_prints_the_environment_lines() {
        let dir = tempdir().unwrap();
        let out = run_setup(&pi_args(), dir.path());
        assert!(out.contains("export OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com"));
        assert!(
            out.contains("export OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-cetra-test")
        );
        assert!(out.contains("export OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true"));
    }

    #[test]
    fn pi_setup_no_content_prints_unset() {
        let dir = tempdir().unwrap();
        let mut args = pi_args();
        args.no_content = true;
        let out = run_setup(&args, dir.path());
        assert!(out.contains("export OTEL_EXPORTER_OTLP_HEADERS="));
        assert!(
            out.contains("unset OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT"),
            "{out}"
        );
        assert!(
            out.contains("Remove the content line from your profile if an earlier setup added it."),
            "{out}"
        );
        assert!(!out.contains("CAPTURE_MESSAGE_CONTENT=true"), "{out}");
    }

    #[test]
    fn pi_setup_uses_pi_coding_agent_dir() {
        let root = tempdir().unwrap();
        let paths = paths_under(root.path());
        let custom = root.path().join("custom-pi");
        assert_eq!(
            pi_dir_from(Some(custom.clone().into_os_string()), &paths),
            custom
        );
        assert_eq!(
            pi_dir_from(Some(OsString::new()), &paths),
            paths.home.join(".pi").join("agent")
        );
        assert_eq!(
            pi_dir_from(None, &paths),
            paths.home.join(".pi").join("agent")
        );
        run_setup(&pi_args(), &custom);
        assert!(extension_path(&custom).is_file());
        assert!(!paths.home.join(".pi").exists());
    }

    #[test]
    fn pi_setup_refuses_a_foreign_file() {
        let dir = tempdir().unwrap();
        write_foreign(dir.path());
        let foreign = fs::read_to_string(extension_path(dir.path())).unwrap();
        let mut out = Vec::new();
        let error = setup_in(&pi_args(), URL, dir.path(), &mut out).unwrap_err();
        assert!(error.to_string().contains("was not written by cetra"));
        assert_eq!(
            fs::read_to_string(extension_path(dir.path())).unwrap(),
            foreign
        );
    }

    #[test]
    fn pi_setup_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let first = run_setup(&pi_args(), dir.path());
        let written = fs::read(extension_path(dir.path())).unwrap();
        let second = run_setup(&pi_args(), dir.path());
        assert_eq!(fs::read(extension_path(dir.path())).unwrap(), written);
        assert!(first.contains("wrote "));
        assert!(!second.contains("wrote "));
    }

    #[test]
    fn pi_setup_leaves_no_backup() {
        let dir = tempdir().unwrap();
        let path = extension_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // An older file cetra wrote: the marker, then other text.
        fs::write(&path, format!("{MARKER}\n// an older version\n")).unwrap();
        let out = run_setup(&pi_args(), dir.path());
        assert_eq!(fs::read_to_string(&path).unwrap(), EXTENSION);
        assert!(out.contains("wrote "), "{out}");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".cetra-backup"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn pi_print_writes_nothing() {
        let dir = tempdir().unwrap();
        let mut args = pi_args();
        args.print = true;
        let out = run_setup(&args, dir.path());
        assert!(!extension_path(dir.path()).exists());
        assert!(out.contains(MARKER));
    }

    #[test]
    fn pi_remove_deletes_only_our_file() {
        let dir = tempdir().unwrap();
        run_setup(&pi_args(), dir.path());
        let out = run_remove(dir.path());
        assert!(!extension_path(dir.path()).exists());
        assert!(out.contains("removed "));

        write_foreign(dir.path());
        let out = run_remove(dir.path());
        assert!(extension_path(dir.path()).exists());
        assert!(out.contains("was not written by cetra"));

        let absent = dir.path().join("absent");
        let out = run_remove(&absent);
        assert!(out.contains("no cetra pi extension found at"));
    }

    #[test]
    fn pi_extension_has_no_imports_outside_node() {
        for line in EXTENSION.lines() {
            let trimmed = line.trim_start();
            let imports = trimmed.starts_with("import ")
                || (trimmed.starts_with("export ") && trimmed.contains(" from "))
                || trimmed.contains("require(");
            if imports {
                assert!(line.contains("\"node:"), "not a node module: {line}");
            }
        }
    }

    #[test]
    fn pi_extension_scope_is_the_detection_key() {
        assert!(EXTENSION.contains("\"cetra-pi\""));
    }

    #[test]
    fn pi_extension_node_tests_pass() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipped: node is not on PATH");
            return;
        }
        let test_file = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agents/pi_extension.test.mjs"
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
