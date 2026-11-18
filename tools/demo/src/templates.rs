//! Loads the trace templates from the captured OTLP fixtures (DO5 step 2).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::info;

use crate::error::DemoError;
use crate::otlp::ResourceSpans;

/// A trace as the generator builds and sends it: the `resourceSpans` of one request.
pub(crate) type Trace = Vec<ResourceSpans>;

/// The fewest templates the generator runs with. Fewer would repeat the same traces too often.
pub(crate) const MIN_TEMPLATES: usize = 10;

/// The groups of fixtures that are not used as templates.
const SKIPPED_GROUPS: &[&str] = &["edge"];

#[derive(Deserialize)]
struct CaseMeta {
    origin: String,
}

#[derive(Deserialize)]
struct Input {
    #[serde(rename = "resourceSpans", default)]
    resource_spans: Vec<ResourceSpans>,
}

/// Loads every captured `input.json` under `dir` (`<group>/<case>/input.json`).
///
/// A case is skipped when its `case.json` `origin` is not `captured`, when a file is
/// missing or does not parse, or when a span has no usable start or end time. The
/// skipped count is logged. It is an error when fewer than [`MIN_TEMPLATES`] remain.
pub(crate) fn load(dir: &Path) -> Result<Vec<Trace>, DemoError> {
    let mut templates = Vec::new();
    let mut skipped = 0_usize;
    for group in subdirs(dir)? {
        if group
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| SKIPPED_GROUPS.contains(&name))
        {
            continue;
        }
        for case in subdirs(&group)? {
            match load_case(&case)? {
                Some(trace) => templates.push(trace),
                None => skipped += 1,
            }
        }
    }
    if templates.len() < MIN_TEMPLATES {
        return Err(DemoError::TooFewTemplates {
            found: templates.len(),
            needed: MIN_TEMPLATES,
        });
    }
    info!(
        templates = templates.len(),
        skipped, "loaded fixture templates"
    );
    Ok(templates)
}

fn load_case(case: &Path) -> Result<Option<Trace>, DemoError> {
    let Some(meta) = read_json::<CaseMeta>(&case.join("case.json"))? else {
        return Ok(None);
    };
    if meta.origin != "captured" {
        return Ok(None);
    }
    let Some(input) = read_json::<Input>(&case.join("input.json"))? else {
        return Ok(None);
    };
    if !usable(&input.resource_spans) {
        return Ok(None);
    }
    Ok(Some(input.resource_spans))
}

/// A template needs at least one span, and every span needs integer start and end times.
fn usable(resource_spans: &[ResourceSpans]) -> bool {
    let mut any = false;
    for span in resource_spans
        .iter()
        .flat_map(|rs| rs.scope_spans.iter())
        .flat_map(|scope| scope.spans.iter())
    {
        if span.start.parse::<i64>().is_err() || span.end.parse::<i64>().is_err() {
            return false;
        }
        any = true;
    }
    any
}

/// The directories directly under `dir`, sorted, so a seeded run is repeatable.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, DemoError> {
    let read_error = |source: std::io::Error| DemoError::Read {
        path: dir.to_path_buf(),
        source,
    };
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// `None` when the file is missing or does not parse (the case is skipped).
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, DemoError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DemoError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    Ok(serde_json::from_str(&text).ok())
}

/// The captured fixtures of the repository, for tests in other modules.
#[cfg(test)]
pub(crate) fn fixture_templates() -> Vec<Trace> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/otlp");
    load(&dir).expect("the repository's fixtures load")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_fixtures_give_enough_templates_and_skip_edge() {
        let templates = fixture_templates();
        assert!(templates.len() >= MIN_TEMPLATES);
        assert!(templates.iter().all(|trace| !trace.is_empty()));
    }

    #[test]
    fn too_few_templates_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = load(dir.path());
        assert!(matches!(
            result,
            Err(DemoError::TooFewTemplates { found: 0, .. })
        ));
    }
}
