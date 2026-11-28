//! Reads of issue occurrences shared by the issue routes, MCP and the dataset route (D-IC-9,
//! D-IC-11): the occurrence counts of an issue and the issues merged into it, and the newest
//! occurrence scores of each issue.

use std::cmp::Reverse;
use std::collections::HashMap;

use cetra_core::enums::ScoreSource;
use cetra_core::ids::{OrgId, ProjectId};
use cetra_core::score::Score;
use cetra_storage::{
    MAX_COUNT_NAMES, QueryContext, ScoreListRequest, ScoreNameCount, ScoreNameCountsRequest,
    TimeWindow,
};
use uuid::Uuid;

use crate::api::convert;
use crate::eval::budget::DAY_NS;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::routes::read::{READ_TIMEOUT, open_query};

/// The default occurrences period, 14 days.
pub(crate) const OCCURRENCE_DEFAULT_PERIOD_NS: i64 = 14 * DAY_NS;
/// Most issues an occurrence read takes examples from: the issue, then the seven merged issues
/// with the most occurrences (D-IC-9).
pub(crate) const MAX_EXAMPLE_NAMES: usize = 8;

/// Occurrence count and user count of a name set (D-IC-9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) count: u64,
    /// The largest user count of the names, not their sum: a user can occur under several.
    pub(crate) users: u64,
}

/// The occurrence score name of an issue (D-IC-11).
pub(crate) fn score_name(issue: Uuid) -> String {
    format!("issue:{issue}")
}

/// The occurrence count of an issue's own name in `counts`; 0 when it has none.
pub(crate) fn issue_count(counts: &HashMap<String, Tally>, issue: Uuid) -> u64 {
    counts
        .get(&score_name(issue))
        .map_or(0, |tally| tally.count)
}

/// Orders merged issues by their occurrence count, most first, then by id (D-IC-9).
pub(crate) fn most_occurrences_first(merged: &mut [Uuid], counts: &HashMap<String, Tally>) {
    merged.sort_by_key(|issue| (Reverse(issue_count(counts, *issue)), *issue));
}

/// The score names an issue counts: its own, then those of the issues merged into it (D-IC-9).
pub(crate) fn names_of(issue: Uuid, merged: &[Uuid]) -> Vec<String> {
    std::iter::once(issue)
        .chain(merged.iter().copied())
        .map(score_name)
        .collect()
}

/// The tally of `names`, from the per-name tallies. Names without occurrences count nothing.
pub(crate) fn total(names: &[String], tallies: &HashMap<String, Tally>) -> Tally {
    names
        .iter()
        .filter_map(|name| tallies.get(name))
        .fold(Tally::default(), |sum, tally| Tally {
            count: sum.count.saturating_add(tally.count),
            users: sum.users.max(tally.users),
        })
}

/// The count rows of `names` in `window`, read in chunks of at most [`MAX_COUNT_NAMES`] names:
/// the storage read refuses more, and an issue can have more names than that once its merged
/// issues are counted. A name is in one chunk only, so its rows are not split.
pub(crate) async fn name_counts(
    ctx: &QueryContext,
    names: &[String],
    window: TimeWindow,
    bucket_ns: Option<i64>,
) -> Result<Vec<ScoreNameCount>, ApiError> {
    let mut rows = Vec::new();
    for chunk in names.chunks(MAX_COUNT_NAMES) {
        rows.extend(
            cetra_storage::score_name_counts(
                ctx,
                ScoreNameCountsRequest {
                    names: chunk,
                    window,
                    bucket_ns,
                    timeout: READ_TIMEOUT,
                },
            )
            .await?,
        );
    }
    Ok(rows)
}

/// Live occurrences of each name in `window`, per name (D-IC-9).
pub(crate) async fn tallies(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    names: &[String],
    window: TimeWindow,
) -> Result<HashMap<String, Tally>, ApiError> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let ctx = open_query(state, org, project, None).await?;
    let counts = name_counts(&ctx, names, window, None).await?;
    Ok(counts
        .into_iter()
        .map(|row| {
            (
                row.name,
                Tally {
                    count: row.count,
                    users: row.users,
                },
            )
        })
        .collect())
}

/// The issues whose occurrences an issue page shows or copies: `id` first, then the merged issues
/// ordered by [`most_occurrences_first`] over `counts`, at most [`MAX_EXAMPLE_NAMES`] in all.
pub(crate) fn example_issues(
    id: Uuid,
    merged: &[Uuid],
    counts: &HashMap<String, Tally>,
) -> Vec<Uuid> {
    let mut by_count = merged.to_vec();
    most_occurrences_first(&mut by_count, counts);
    let mut issues = vec![id];
    issues.extend(by_count);
    issues.truncate(MAX_EXAMPLE_NAMES);
    issues
}

/// The newest occurrence scores of each issue in `issues` (name `issue:<id>`, source `issue`),
/// up to `per_issue` each, as `(issue, score)` pairs, issue by issue in the given order and
/// newest first inside an issue. One storage read per issue. `window` limits the occurrences to
/// those in it (`None`: all).
pub(crate) async fn occurrence_scores(
    ctx: &QueryContext,
    project: ProjectId,
    issues: &[Uuid],
    per_issue: usize,
    window: Option<TimeWindow>,
) -> Result<Vec<(Uuid, Score)>, ApiError> {
    let mut out = Vec::new();
    for &issue in issues {
        let name = score_name(issue);
        let page = cetra_storage::score_list(
            ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: Some(name.as_str()),
                source: Some(ScoreSource::Issue),
                evaluator_id: None,
                rule_id: None,
                queue_id: None,
                window,
                limit: per_issue,
                cursor: None,
                timeout: READ_TIMEOUT,
            },
        )
        .await?;
        let scores = convert::scores(&page.batches, project)?;
        out.extend(scores.into_iter().map(|score| (issue, score)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use cetra_core::score::ScoreTarget;

    use super::*;
    use crate::meta::issues::SourceKind;
    use crate::test_support::TestApp;
    use crate::test_support::issues::{occurrence, put_source, source_row};

    const HOUR_NS: i64 = 3_600_000_000_000;

    /// A valid trace id of `n`, distinct for each `n` above 0.
    fn trace(n: u128) -> String {
        format!("{n:032x}")
    }

    /// The trace of an occurrence score.
    fn trace_of(score: &Score) -> String {
        match &score.target {
            ScoreTarget::Trace { trace_id } => trace_id.to_string(),
            _ => panic!("an occurrence targets a trace"),
        }
    }

    fn tally(count: u64, users: u64) -> Tally {
        Tally { count, users }
    }

    #[tokio::test]
    async fn occurrence_scores_returns_newest_first_per_issue_up_to_the_limit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "Errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        // Five occurrences, the trace of hour `h` written `h` hours ago.
        for hours in 1..=5 {
            occurrence(
                &app,
                &seeded,
                source_id,
                issue,
                &trace(hours as u128),
                now - hours * HOUR_NS,
                None,
                None,
            )
            .await;
        }
        let ctx = open_query(&app.state, seeded.org_id, project, None)
            .await
            .unwrap();

        let pairs = occurrence_scores(&ctx, project, &[issue], 3, None)
            .await
            .unwrap();

        let got: Vec<(Uuid, String)> = pairs
            .iter()
            .map(|(id, score)| (*id, trace_of(score)))
            .collect();
        assert_eq!(
            got,
            vec![(issue, trace(1)), (issue, trace(2)), (issue, trace(3)),]
        );
    }

    #[tokio::test]
    async fn occurrence_scores_keeps_the_order_of_the_given_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "Errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        occurrence(
            &app,
            &seeded,
            source_id,
            a,
            &trace(1),
            now - HOUR_NS,
            None,
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            a,
            &trace(2),
            now - 2 * HOUR_NS,
            None,
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            b,
            &trace(3),
            now - HOUR_NS,
            None,
            None,
        )
        .await;
        let ctx = open_query(&app.state, seeded.org_id, project, None)
            .await
            .unwrap();

        let pairs = occurrence_scores(&ctx, project, &[b, a], 3, None)
            .await
            .unwrap();

        let order: Vec<Uuid> = pairs.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![b, a, a]);
    }

    #[tokio::test]
    async fn occurrence_scores_of_an_issue_without_occurrences_is_empty() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let ctx = open_query(&app.state, seeded.org_id, project, None)
            .await
            .unwrap();

        let pairs = occurrence_scores(&ctx, project, &[Uuid::now_v7()], 3, None)
            .await
            .unwrap();

        assert!(pairs.is_empty());
    }

    #[test]
    fn names_of_puts_the_issue_first() {
        let (issue, a, b) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        assert_eq!(
            names_of(issue, &[a, b]),
            vec![score_name(issue), score_name(a), score_name(b)]
        );
    }

    #[test]
    fn total_sums_counts_and_takes_the_largest_user_count() {
        let tallies = HashMap::from([("x".to_owned(), tally(2, 1)), ("y".to_owned(), tally(3, 2))]);
        let names = ["x", "y", "missing"].map(str::to_owned);

        assert_eq!(total(&names, &tallies), tally(5, 2));
    }

    #[test]
    fn example_issues_puts_the_issue_first_then_the_busiest_merged_ones() {
        let id = Uuid::now_v7();
        let merged: Vec<Uuid> = (0..9).map(|_| Uuid::now_v7()).collect();
        // Merged issue `i` has `i + 1` occurrences: the seven busiest are `merged[2..9]`.
        let counts: HashMap<String, Tally> = merged
            .iter()
            .enumerate()
            .map(|(i, issue)| (score_name(*issue), tally(i as u64 + 1, 1)))
            .collect();
        let mut expected = vec![id];
        expected.extend(merged[2..9].iter().rev().copied());

        assert_eq!(example_issues(id, &merged, &counts), expected);
    }

    #[test]
    fn example_issues_orders_equal_counts_by_id_and_returns_fewer_than_the_limit() {
        let id = Uuid::now_v7();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let counts = HashMap::from([(score_name(a), tally(4, 1)), (score_name(b), tally(4, 1))]);
        let mut ties = vec![a, b];
        ties.sort();
        let mut expected = vec![id];
        expected.extend(ties);

        assert_eq!(example_issues(id, &[a, b], &counts), expected);
    }
}
