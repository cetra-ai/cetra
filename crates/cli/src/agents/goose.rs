//! Goose (D-IN-29 item 5): `setup` writes nothing and prints the environment lines Goose reads
//! its endpoint, key and content switch from (Copilot's pattern); `remove` reminds the user to
//! delete them. Goose has no hook and no `agents.toml`.

use std::io::Write;
use std::path::Path;

use super::SetupArgs;
use super::setup::shell_quote;

/// `cetra agents setup goose`. Results and notes go to `out`. Nothing is written, with or
/// without `--print`.
pub(super) fn setup(args: &SetupArgs, url: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(
        out,
        "Goose reads its endpoint, key and content switch from the environment. Add these lines to your shell profile:"
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
    if !args.no_content {
        writeln!(
            out,
            "export OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true"
        )?;
    }
    writeln!(out, "export OTEL_LOGS_EXPORTER=none")?;
    writeln!(out, "export OTEL_METRICS_EXPORTER=none")?;
    Ok(())
}

/// `cetra agents remove goose`. Results and notes go to `out`.
pub(super) fn remove(out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(
        out,
        "nothing to remove: cetra agents setup goose writes no file. Delete the OTEL_* lines it printed from your shell profile."
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use tempfile::tempdir;

    use super::*;
    use crate::agents::Agent;
    use crate::agents::Paths;
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

    fn goose_args() -> SetupArgs {
        SetupArgs {
            agent: Agent::Goose,
            url: URL.to_owned(),
            key: Token::from_str("sk-cetra-test").unwrap(),
            user: None,
            no_content: false,
            print: false,
        }
    }

    fn run_setup(args: &SetupArgs, paths: &Paths) -> String {
        let mut out = Vec::new();
        agents_setup(args, paths, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn run_remove() -> String {
        let mut out = Vec::new();
        remove(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn goose_setup_prints_the_lines_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let out = run_setup(&goose_args(), &paths);
        assert!(
            out.contains(
                "Goose reads its endpoint, key and content switch from the environment. Add these lines to your shell profile:"
            ),
            "{out}"
        );
        let lines: Vec<&str> = out.lines().collect();
        let expected = [
            "export OTEL_EXPORTER_OTLP_ENDPOINT=https://cetra.example.com",
            "export OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20sk-cetra-test",
            "export OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true",
            "export OTEL_LOGS_EXPORTER=none",
            "export OTEL_METRICS_EXPORTER=none",
        ];
        let mut previous = None;
        for line in expected {
            let at = lines.iter().position(|kept| kept == &line);
            let Some(at) = at else {
                panic!("{line} is missing from {out:?}");
            };
            if let Some(previous) = previous {
                assert!(previous < at, "{out:?}");
            }
            previous = Some(at);
        }
        assert!(!paths.home.exists());
        assert!(!paths.agents_file().exists());
    }

    #[test]
    fn goose_no_content_omits_the_content_line() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut args = goose_args();
        args.no_content = true;
        let out = run_setup(&args, &paths);
        assert!(!out.contains("CAPTURE_MESSAGE_CONTENT"), "{out}");
        assert!(out.contains("export OTEL_LOGS_EXPORTER=none"), "{out}");
        assert!(out.contains("export OTEL_METRICS_EXPORTER=none"), "{out}");
    }

    #[test]
    fn goose_remove_prints_the_reminder() {
        let out = run_remove();
        assert_eq!(
            out,
            "nothing to remove: cetra agents setup goose writes no file. Delete the OTEL_* lines it printed from your shell profile.\n"
        );
    }

    #[test]
    fn goose_remove_through_the_dispatcher_prints_the_reminder() {
        let dir = tempdir().unwrap();
        let paths = paths_under(dir.path());
        let mut out = Vec::new();
        agents_remove(Agent::Goose, &paths, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("nothing to remove")
        );
    }
}
