//! The org's data window on every read (D175, D-P-21, D-P-44).

use llmobs_core::ids::OrgId;
use llmobs_storage::QueryOptions;
#[cfg(test)]
use std::path::Path;

use crate::http::AppState;

const DAY_NS: i64 = 86_400_000_000_000;

/// Which rows a read may see. Built only by [`Visibility::window`] and [`Visibility::exempt`]
/// (D-P-44): the inner enum is private, so no other file can name a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visibility(Inner);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inner {
    /// Rows before the instant (ns) are hidden. `Window(None)`: no window.
    Window(Option<i64>),
    /// Every row.
    Exempt,
}

impl Visibility {
    /// The org's window now: `now − data_access_days`, or no bound without a limit.
    pub(crate) fn window(state: &AppState, org: OrgId) -> Self {
        let now = state.clock.now_ns();
        Self(Inner::Window(
            super::of(state, org)
                .limits
                .data_access_days
                .map(|days| visible_from(now, days)),
        ))
    }

    /// Every row, whatever the window. For deletion reads only (D-P-21): `deletion/resolve.rs`
    /// and `routes/deletions.rs`. Never use it for a read that returns data to a caller.
    pub(crate) fn exempt() -> Self {
        Self(Inner::Exempt)
    }

    /// No window, for tests that read rows of a fixed past.
    #[cfg(test)]
    pub(crate) fn unlimited() -> Self {
        Self(Inner::Window(None))
    }

    /// The bound a query hides rows before; `None` for no window and for an exempt read.
    pub(crate) fn visible_from(self) -> Option<i64> {
        match self.0 {
            Inner::Window(from) => from,
            Inner::Exempt => None,
        }
    }
}

/// `now − days`, saturating (a token's huge window never overflows).
fn visible_from(now: i64, days: u64) -> i64 {
    let span = i64::try_from(days)
        .unwrap_or(i64::MAX)
        .saturating_mul(DAY_NS);
    now.saturating_sub(span)
}

/// The only code in the server that builds [`QueryOptions`] (D-P-44).
pub(crate) fn query_options(visibility: Visibility, around: Option<i64>) -> QueryOptions {
    QueryOptions::new(around, visibility.visible_from())
}

/// The `.rs` files under `src/`, as paths relative to it with `/` separators, with their text
/// and with all whitespace removed.
#[cfg(test)]
fn source_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("src is readable") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path
                    .strip_prefix(root)
                    .expect("under src")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path).expect("source is readable");
                out.push((relative, text));
            }
        }
    }
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort();
    files
}

/// The files whose text (whitespace removed) contains `needle`, for an invariant test.
#[cfg(test)]
fn files_containing(needle: &str) -> Vec<String> {
    source_files()
        .into_iter()
        .filter(|(_, text)| {
            let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            squeezed.contains(needle)
        })
        .map(|(path, _)| path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestApp, set_limits};
    use std::time::Duration;

    /// 2026-01-01T00:00:00Z, the manual clock of the tests.
    const NOW: i64 = 1_767_225_600_000_000_000;

    fn assert_subset(found: &[String], allowed: &[&str], what: &str) {
        for path in found {
            assert!(
                allowed.contains(&path.as_str()),
                "{what}: {path} is not in {allowed:?}"
            );
        }
    }

    #[test]
    fn only_query_options_builds_query_options() {
        let needles = [
            concat!("QueryOptions", "::new("),
            concat!("QueryOptions", "{"),
        ];
        for needle in needles {
            let offending: Vec<String> = files_containing(needle)
                .into_iter()
                .filter(|path| path != "entitlements/window.rs")
                .collect();
            assert!(
                offending.is_empty(),
                "files build QueryOptions directly (use query_options): {offending:?}"
            );
        }
    }

    #[test]
    fn exempt_reads_are_deletion_reads() {
        let found = files_containing(concat!("Visibility", "::exempt("));
        assert_subset(
            &found,
            &[
                "entitlements/window.rs",
                "deletion/resolve.rs",
                "routes/deletions.rs",
            ],
            "Visibility::exempt( outside deletion reads",
        );
        for required in ["deletion/resolve.rs", "routes/deletions.rs"] {
            assert!(
                found.iter().any(|path| path == required),
                "{required} must keep the deletion exemption"
            );
        }
    }

    #[test]
    fn no_unwindowed_visibility() {
        let found = files_containing(concat!("Visibility", "::unlimited("));
        assert_subset(
            &found,
            &["entitlements/window.rs", "test_support/mod.rs"],
            "Visibility::unlimited( outside window.rs and test_support",
        );
    }

    #[test]
    fn direct_storage_reads_are_listed() {
        let found = files_containing(concat!("storage", ".query("));
        assert_subset(
            &found,
            &[
                "routes/read.rs",
                "deletion/resolve.rs",
                "eval/trace_load.rs",
                "test_support/mod.rs",
            ],
            "direct storage reads",
        );
    }

    #[test]
    fn visible_from_saturates() {
        assert_eq!(visible_from(NOW, 30), NOW - 30 * DAY_NS);
        assert_eq!(visible_from(NOW, u64::MAX), NOW.saturating_sub(i64::MAX));
        assert_eq!(visible_from(i64::MIN, 1), i64::MIN);
    }

    #[tokio::test]
    async fn window_follows_the_org() {
        let app = TestApp::new().await;
        let org = app.seed().await.org_id;
        assert_eq!(Visibility::window(&app.state, org), Visibility::unlimited());

        set_limits(
            &app,
            org,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        let first = Visibility::window(&app.state, org);
        assert_eq!(first, Visibility(Inner::Window(Some(NOW - 30 * DAY_NS))));

        app.state.clock.advance(Duration::from_secs(3600));
        let later = Visibility::window(&app.state, org);
        assert_eq!(
            later.visible_from().expect("bound") - first.visible_from().expect("bound"),
            3_600_000_000_000
        );
    }
}
