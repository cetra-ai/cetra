//! `cetra debug-bundle` (track TL § 4): writes the bundle `cetra_server::observability::bundle` renders, to a new file or to stdout, then prints the review notice. Contacts no network.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use cetra_server::observability::bundle::{self, REVIEW_NOTICE};

/// Writes the bundle to `output` (default: `cetra-debug-<UTC time>.json` in the current
/// directory; `-` is stdout) and prints where, then [`REVIEW_NOTICE`]. With `-`, the notice
/// goes to `err` so `out` holds the JSON only.
pub(crate) async fn run(
    config: Option<&Path>,
    output: Option<&Path>,
    env: &[(String, String)],
    ui_embedded: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let bytes = bundle::render(config, env, ui_embedded, now)
        .await
        .context("building the debug bundle")?;
    if output == Some(Path::new("-")) {
        out.write_all(&bytes)?;
        out.flush()?;
        writeln!(err, "{REVIEW_NOTICE}")?;
        return Ok(());
    }
    let path = output.map_or_else(|| PathBuf::from(bundle::file_name(now)), Path::to_path_buf);
    let path = std::path::absolute(&path).context("resolving the output path")?;
    bundle::write_new(&path, &bytes)?;
    writeln!(out, "wrote {}", path.display())?;
    writeln!(out, "{REVIEW_NOTICE}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use cetra_server::observability::bundle::REVIEW_NOTICE;

    use super::run;

    /// Writes a `cetra.toml` into `dir` and returns its path.
    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("cetra.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A config whose data directory is `dir/data` (created, empty: no server runs).
    fn config_for(dir: &Path) -> PathBuf {
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        write_config(
            dir,
            &format!("data_dir = {:?}\n", data.display().to_string()),
        )
    }

    #[tokio::test]
    async fn it_writes_a_reviewable_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let out_path = dir.path().join("bundle.json");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(
            Some(&config),
            Some(&out_path),
            &[],
            false,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["server"]["state"], "not_running");
        let expected = format!("wrote {}\n{REVIEW_NOTICE}\n", out_path.display());
        assert_eq!(String::from_utf8(out).unwrap(), expected);
        assert!(err.is_empty());
    }

    #[tokio::test]
    async fn dash_writes_the_json_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(
            Some(&config),
            Some(Path::new("-")),
            &[],
            false,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(document["schema"], 1);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("review it before you share it")
        );
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            !files
                .iter()
                .any(|name| name.to_string_lossy().starts_with("cetra-debug-"))
        );
        assert!(!files.iter().any(|name| name == "bundle.json"));
    }

    #[tokio::test]
    async fn an_existing_output_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let out_path = dir.path().join("bundle.json");
        std::fs::write(&out_path, "x").unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let error = run(
            Some(&config),
            Some(&out_path),
            &[],
            false,
            &mut out,
            &mut err,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("already exists"));
        assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "x");
    }

    #[tokio::test]
    async fn an_invalid_config_still_writes_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let config = write_config(
            dir.path(),
            &format!(
                "data_dir = {:?}\nhttp_addr = \"nope\"\n",
                data.display().to_string()
            ),
        );
        let out_path = dir.path().join("bundle.json");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(
            Some(&config),
            Some(&out_path),
            &[],
            false,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out_path).unwrap()).unwrap();
        assert_eq!(document["config_error"]["key"], "http_addr");
    }
}
