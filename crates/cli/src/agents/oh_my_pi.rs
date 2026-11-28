//! Oh My Pi (D-IN-29 item 6): `setup` writes the `OTEL_*` lines Oh My Pi reads its endpoint,
//! key and content switch from into `~/.omp/agent/.env`, the dotenv file only Oh My Pi reads;
//! `remove` takes the lines back when they point at the server saved in `agents.toml`. Oh My Pi
//! has no hook, but saves `agents.toml`: its `.env` lines have no marker, so `remove` must check
//! they point at the saved server (D-IN-29 item 4).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;

use super::setup::{
    NOT_FOUND_NOTE, OTHER_SERVER_NOTE, emit, env_with_var, env_without_line, read_optional_text,
    write_settings,
};
use super::{AgentsFile, Paths, SetupArgs};

/// `<home>/.omp/agent/.env`, one of the dotenv files Oh My Pi reads its `OTEL_*` variables
/// from. A project `.env` would carry them too, but setup never writes there (D-IN-29 item 6).
fn omp_env_path(paths: &Paths) -> PathBuf {
    paths.home.join(".omp").join("agent").join(".env")
}

/// The `(name, line)` pairs setup writes (D-IN-29 item 6), in order. The header value has a
/// plain space: the dotenv form Oh My Pi's reader takes, not the percent-encoded shell one.
fn omp_lines(url: &str, key: &str, capture: bool) -> Vec<(&'static str, String)> {
    let mut lines = vec![
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("OTEL_EXPORTER_OTLP_ENDPOINT={url}"),
        ),
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer {key}"),
        ),
        ("OTEL_SERVICE_NAME", "OTEL_SERVICE_NAME=omp".to_owned()),
    ];
    if capture {
        lines.push((
            "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT",
            "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true".to_owned(),
        ));
    }
    lines.push(("OTEL_LOGS_EXPORTER", "OTEL_LOGS_EXPORTER=none".to_owned()));
    lines.push((
        "OTEL_METRICS_EXPORTER",
        "OTEL_METRICS_EXPORTER=none".to_owned(),
    ));
    lines
}

/// `cetra agents setup oh-my-pi`. Results and notes go to `out`. A backup of the `.env` is
/// kept: the file may hold the user's own lines.
pub(super) fn setup(
    args: &SetupArgs,
    url: &str,
    paths: &Paths,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let path = omp_env_path(paths);
    let text = read_optional_text(&path)?.unwrap_or_default();
    let (env, replaced_other) = omp_lines(url, args.key.expose(), !args.no_content)
        .into_iter()
        .fold((text, false), |(text, replaced), (name, line)| {
            let (text, line_replaced) = env_with_var(&text, name, &line);
            (text, replaced || line_replaced)
        });
    emit(&path, env.as_bytes(), args.print, out)?;
    if replaced_other && !args.print {
        writeln!(out, "replaced existing OTEL_* lines in {}", path.display())?;
    }
    writeln!(
        out,
        "Oh My Pi reads these from its own .env file; no shell profile change is needed."
    )?;
    Ok(())
}

/// `cetra agents remove oh-my-pi`. Results and notes go to `out`. Removes the lines setup
/// wrote, and deletes the `.env` when they were all it held. Accepted: a user's own
/// `OTEL_SERVICE_NAME=omp`, `OTEL_LOGS_EXPORTER=none` or `OTEL_METRICS_EXPORTER=none` line is
/// removed too (the lines have no marker), and when another agent's setup later saved a
/// different key in the shared `agents.toml`, the old header line stays, as
/// `settings_env_remove` does for Gemini.
pub(super) fn remove(paths: &Paths, out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(file) = AgentsFile::load(&paths.agents_file())? else {
        writeln!(out, "{NOT_FOUND_NOTE}")?;
        return Ok(());
    };
    let path = omp_env_path(paths);
    let Some(text) = read_optional_text(&path)? else {
        writeln!(out, "nothing to remove")?;
        return Ok(());
    };
    let endpoint = format!("OTEL_EXPORTER_OTLP_ENDPOINT={}", file.url);
    if env_without_line(&text, &endpoint).is_none() {
        if text.lines().any(|line| {
            line.trim_end_matches('\r')
                .starts_with("OTEL_EXPORTER_OTLP_ENDPOINT=")
        }) {
            writeln!(out, "{OTHER_SERVER_NOTE}")?;
        } else {
            writeln!(out, "nothing to remove")?;
        }
        return Ok(());
    }
    let mut rest = text;
    for (_, line) in omp_lines(&file.url, &file.key, true) {
        if let Some(without) = env_without_line(&rest, &line) {
            rest = without;
        }
    }
    if rest.trim().is_empty() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        writeln!(out, "removed {}", path.display())?;
    } else if write_settings(&path, rest.as_bytes(), false)? {
        // No backup: one of the file as setup left it would keep the key on disk.
        writeln!(out, "updated {}", path.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
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

    fn omp_args() -> SetupArgs {
        SetupArgs {
            agent: Agent::OhMyPi,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: None,
            no_content: false,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, paths: &Paths) -> String {
        let mut out = Vec::new();
        setup(args, URL, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove(paths: &Paths) -> String {
        let mut out = Vec::new();
        remove(paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn env_path(paths: &Paths) -> PathBuf {
        omp_env_path(paths)
    }

    fn write_env(paths: &Paths, text: &str) {
        let path = env_path(paths);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
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
    fn omp_setup_writes_the_env_lines_in_order() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let out = run_setup(&omp_args(), &paths);
        assert_eq!(
            fs::read_to_string(env_path(&paths)).unwrap(),
            "OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com\n\
             OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer sk-cetra-test\n\
             OTEL_SERVICE_NAME=omp\n\
             OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true\n\
             OTEL_LOGS_EXPORTER=none\n\
             OTEL_METRICS_EXPORTER=none\n"
        );
        assert!(
            out.contains(&format!("wrote {}", env_path(&paths).display())),
            "{out}"
        );
        assert!(
            out.contains(
                "Oh My Pi reads these from its own .env file; no shell profile change is needed."
            ),
            "{out}"
        );
    }

    #[test]
    fn omp_setup_keeps_other_lines_and_replaces_ours() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        write_env(&paths, "FOO=1\nOTEL_EXPORTER_OTLP_ENDPOINT=http://old\n");
        let out = run_setup(&omp_args(), &paths);
        assert_eq!(
            fs::read_to_string(env_path(&paths)).unwrap(),
            "FOO=1\n\
             OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com\n\
             OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer sk-cetra-test\n\
             OTEL_SERVICE_NAME=omp\n\
             OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true\n\
             OTEL_LOGS_EXPORTER=none\n\
             OTEL_METRICS_EXPORTER=none\n"
        );
        assert!(
            out.contains(&format!(
                "replaced existing OTEL_* lines in {}",
                env_path(&paths).display()
            )),
            "{out}"
        );
    }

    #[test]
    fn omp_setup_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let first = run_setup(&omp_args(), &paths);
        let written = fs::read(env_path(&paths)).unwrap();
        let second = run_setup(&omp_args(), &paths);
        assert_eq!(fs::read(env_path(&paths)).unwrap(), written);
        assert!(first.contains("wrote "), "{first}");
        assert!(!second.contains("wrote "), "{second}");
        assert!(!second.contains("replaced existing"), "{second}");
    }

    #[test]
    fn omp_no_content_omits_the_content_line() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = omp_args();
        args.no_content = true;
        run_setup(&args, &paths);
        let env = fs::read_to_string(env_path(&paths)).unwrap();
        assert!(!env.contains("CAPTURE_MESSAGE_CONTENT"), "{env:?}");
        assert_eq!(env.lines().count(), 5, "{env:?}");
    }

    #[test]
    fn omp_print_writes_nothing() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = omp_args();
        args.print = true;
        let out = run_setup(&args, &paths);
        assert!(!env_path(&paths).exists());
        assert!(
            out.contains(&format!("# {}", env_path(&paths).display())),
            "{out}"
        );
        assert!(
            out.contains("OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer sk-cetra-test"),
            "{out}"
        );
    }

    #[test]
    fn omp_remove_removes_our_lines_and_an_emptied_file() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        run_setup(&omp_args(), &paths);
        save_agents(&paths);
        let out = run_remove(&paths);
        assert!(!env_path(&paths).exists(), "{out}");
        assert!(
            out.contains(&format!("removed {}", env_path(&paths).display())),
            "{out}"
        );
    }

    #[test]
    fn omp_remove_keeps_other_lines() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        write_env(&paths, "FOO=1\n");
        run_setup(&omp_args(), &paths);
        save_agents(&paths);
        let out = run_remove(&paths);
        assert_eq!(
            fs::read_to_string(env_path(&paths)).unwrap(),
            "FOO=1\n",
            "{out}"
        );
        assert!(
            out.contains(&format!("updated {}", env_path(&paths).display())),
            "{out}"
        );
        for entry in fs::read_dir(env_path(&paths).parent().unwrap()).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".cetra-backup")
                    || !fs::read_to_string(entry.path())
                        .unwrap_or_default()
                        .contains("sk-cetra-"),
                "{} holds the key",
                entry.path().display()
            );
        }
    }

    #[test]
    fn omp_remove_leaves_another_server() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let original = "OTEL_EXPORTER_OTLP_ENDPOINT=https://other.example\nFOO=1\n";
        write_env(&paths, original);
        save_agents(&paths);
        let out = run_remove(&paths);
        assert!(out.contains("another server"), "{out}");
        assert_eq!(fs::read_to_string(env_path(&paths)).unwrap(), original);

        // An .env with no endpoint line at all is left alone too.
        write_env(&paths, "FOO=1\n");
        let out = run_remove(&paths);
        assert!(out.contains("nothing to remove"), "{out}");
        assert_eq!(fs::read_to_string(env_path(&paths)).unwrap(), "FOO=1\n");
    }

    #[test]
    fn omp_remove_without_agents_toml_or_env() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        // No agents.toml: nothing changes, whatever the .env holds.
        write_env(
            &paths,
            "OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com\n",
        );
        let out = run_remove(&paths);
        assert!(out.contains("agents.toml not found"), "{out}");
        assert!(env_path(&paths).exists());

        // agents.toml, but no .env.
        save_agents(&paths);
        fs::remove_file(env_path(&paths)).unwrap();
        let out = run_remove(&paths);
        assert_eq!(out, "nothing to remove\n");
    }
}
