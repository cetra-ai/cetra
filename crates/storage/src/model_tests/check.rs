//! The checks after each op: storage's reads against the model's expected rows
//! (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! Each read is a [`Read`]. [`observe`] runs it on one snapshot of a storage and [`compare`]
//! checks the result against a model state, with no panic on a difference, so the reader threads
//! of the concurrent mode can use them too (docs/plans/r-hardening/06-m6-concurrency.md § R6.3).
//! [`check_read`] runs both on the driver thread and panics with the op index, the read, and the
//! expected and observed values. proptest prints the shrunk op list.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cetra_column::ScalarValue;
use cetra_core::schema::{scores, spans};

use super::harness::Harness;
use super::model::{ExpectedScore, ExpectedTrace, Model, ProjectModel};
use super::observe::{
    ALL_ROWS, Kpi, KpiName, LatestScore, ReadFailure, SessionRow, context, observe_alert,
    observe_filter_trace_ids, observe_kpis, observe_latest, observe_scores, observe_scores_by_id,
    observe_sessions, observe_span_detail, observe_spans, observe_trace_detail, observe_traces,
    observe_users, window_of,
};
use super::ops::{SPANS, TRACES, score_key_space};
use super::shape::{DAY_NS, P, ScoreKey, day, hex, midnight, score_id, span_id, trace_id};
use crate::Storage;
use crate::catalog::FileTable;
use crate::deletion::DeletionStatus;
use crate::reads::{AlertMeasure, AlertValue, ListScope, MAX_PAGE};
use crate::test_support::{
    Call, collect_orphans_dry_run, obsolete_rows, orphan_objects, statam_rows, trace_ids_of,
};
use crate::wal::Table;

/// Visible copies per span key, as `ProjectModel::visible_spans` returns them.
type Visible = BTreeMap<(u8, u8), Vec<i64>>;

/// One read the checks and the reader threads run. `observe` runs it on one snapshot; `compare`
/// checks the result against a model state.
#[derive(Clone)]
pub(super) enum Read {
    Spans(P),
    /// Every trace row, unwindowed, evaluation traces kept.
    Traces(P),
    /// The trace list over days D+1..D+2, evaluation traces kept.
    Windowed(P),
    /// The trace list without evaluation traces.
    NoEval(P),
    /// `trace_detail` of trace number `.1`.
    TraceDetail(P, u8),
    /// `span_detail` of span `.2` of trace `.1`.
    SpanDetail(P, u8, u8),
    Scores(P),
    /// `scores_by_id` of the ids given. M1's checks ask for every id of the lanes' key space, so
    /// ids the model hides or never saw are tested too.
    ScoresById(P, Vec<String>),
    Latest(P),
    Sessions(P),
    Users(P),
    /// The KPIs over a window of whole days (`days`).
    Metrics(P, (i64, i64)),
    /// The trace ids `filter_trace_ids` returns over a window of whole days.
    FilterTraceIds(P, (i64, i64)),
    /// One `alert_value` over a window of whole days.
    AlertValue(P, (i64, i64), AlertMeasure),
}

impl fmt::Debug for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Read::Spans(project) => f.debug_tuple("Spans").field(project).finish(),
            Read::Traces(project) => f.debug_tuple("Traces").field(project).finish(),
            Read::Windowed(project) => f.debug_tuple("Windowed").field(project).finish(),
            Read::NoEval(project) => f.debug_tuple("NoEval").field(project).finish(),
            Read::TraceDetail(project, trace) => f
                .debug_tuple("TraceDetail")
                .field(project)
                .field(trace)
                .finish(),
            Read::SpanDetail(project, trace, span_no) => f
                .debug_tuple("SpanDetail")
                .field(project)
                .field(trace)
                .field(span_no)
                .finish(),
            Read::Scores(project) => f.debug_tuple("Scores").field(project).finish(),
            // The ids of a read over every key of the lanes run to hundreds: print their count.
            Read::ScoresById(project, ids) => f
                .debug_tuple("ScoresById")
                .field(project)
                .field(&format_args!("{} ids", ids.len()))
                .finish(),
            Read::Latest(project) => f.debug_tuple("Latest").field(project).finish(),
            Read::Sessions(project) => f.debug_tuple("Sessions").field(project).finish(),
            Read::Users(project) => f.debug_tuple("Users").field(project).finish(),
            Read::Metrics(project, window) => f
                .debug_tuple("Metrics")
                .field(project)
                .field(window)
                .finish(),
            Read::FilterTraceIds(project, window) => f
                .debug_tuple("FilterTraceIds")
                .field(project)
                .field(window)
                .finish(),
            Read::AlertValue(project, window, measure) => f
                .debug_tuple("AlertValue")
                .field(project)
                .field(window)
                .field(measure)
                .finish(),
        }
    }
}

/// What a read returned: one variant per kind of read.
#[derive(Debug, Clone)]
pub(super) enum Observed {
    Spans(Vec<(i64, String, String)>),
    Traces(Vec<ExpectedTrace>),
    /// Boxed: the row is much larger than the other variants' payloads.
    TraceDetail(Option<Box<(ExpectedTrace, Vec<String>)>>),
    SpanDetail(Option<i64>),
    Scores(Vec<ExpectedScore>),
    Latest(Vec<LatestScore>),
    Sessions(Vec<SessionRow>),
    Users(Vec<(String, i64)>),
    Kpis(Vec<Kpi>),
    TraceIds(Vec<String>),
    Alert(AlertValue),
}

/// Runs `read` on one snapshot of `storage`, reading every page of it from that snapshot.
pub(super) async fn observe(storage: &Storage, read: &Read) -> Result<Observed, ReadFailure> {
    Ok(match read {
        Read::Spans(project) => {
            let ctx = context(storage, *project).await?;
            Observed::Spans(observe_spans(&ctx, MAX_PAGE).await?)
        }
        Read::Traces(project) => {
            let ctx = context(storage, *project).await?;
            Observed::Traces(observe_traces(&ctx, ALL_ROWS, MAX_PAGE).await?)
        }
        Read::Windowed(project) => {
            let ctx = context(storage, *project).await?;
            Observed::Traces(
                observe_traces(
                    &ctx,
                    ListScope {
                        window: Some(window_of(days(1, 2))),
                        include_eval: true,
                        include_experiments: true,
                    },
                    MAX_PAGE,
                )
                .await?,
            )
        }
        Read::NoEval(project) => {
            let ctx = context(storage, *project).await?;
            Observed::Traces(
                observe_traces(
                    &ctx,
                    ListScope {
                        window: None,
                        include_eval: false,
                        include_experiments: true,
                    },
                    MAX_PAGE,
                )
                .await?,
            )
        }
        Read::TraceDetail(project, trace) => Observed::TraceDetail(
            observe_trace_detail(storage, *project, *trace)
                .await?
                .map(Box::new),
        ),
        Read::SpanDetail(project, trace, span_no) => {
            Observed::SpanDetail(observe_span_detail(storage, *project, *trace, *span_no).await?)
        }
        Read::Scores(project) => {
            let ctx = context(storage, *project).await?;
            Observed::Scores(observe_scores(&ctx, MAX_PAGE).await?)
        }
        Read::ScoresById(project, ids) => {
            Observed::Scores(observe_scores_by_id(storage, *project, ids).await?)
        }
        Read::Latest(project) => Observed::Latest(observe_latest(storage, *project).await?),
        Read::Sessions(project) => Observed::Sessions(observe_sessions(storage, *project).await?),
        Read::Users(project) => Observed::Users(observe_users(storage, *project).await?),
        Read::Metrics(project, window) => {
            Observed::Kpis(observe_kpis(storage, *project, *window).await?)
        }
        Read::FilterTraceIds(project, window) => {
            Observed::TraceIds(observe_filter_trace_ids(storage, *project, *window).await?)
        }
        Read::AlertValue(project, window, measure) => {
            Observed::Alert(observe_alert(storage, *project, *window, measure.clone()).await?)
        }
    })
}

/// How strictly a span list's keys are compared with the model (`compare_span_keys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Strictness {
    /// M1: nothing runs in the background, so each visible span key is listed exactly once.
    Exact,
    /// M6: a background flush or compaction may not have merged a re-sent copy yet, so a key is
    /// listed up to its visible copies (§ The rule).
    Concurrent,
}

/// Compares `observed`, what `read` returned, with the state of `model` that `read` names. `Err`
/// holds a printable description of the first difference (expected and observed).
pub(super) fn compare(
    model: &Model,
    read: &Read,
    observed: &Observed,
    strictness: Strictness,
) -> Result<(), String> {
    match (read, observed) {
        (Read::Spans(project), Observed::Spans(rows)) => {
            compare_span_keys(model.project(*project), rows, strictness)
        }
        (Read::Traces(project), Observed::Traces(rows)) => {
            compare_traces(model.project(*project), rows)
        }
        (Read::Windowed(project), Observed::Traces(rows)) => {
            let state = model.project(*project);
            let expected = state.traces_in(Some(days(1, 2)), true);
            compare_trace_rows(state, rows, &expected, "the trace list over days D+1..D+2")
        }
        (Read::NoEval(project), Observed::Traces(rows)) => {
            let state = model.project(*project);
            let expected = state.traces_in(None, false);
            compare_trace_rows(
                state,
                rows,
                &expected,
                "the trace list without evaluation traces",
            )
        }
        (Read::TraceDetail(project, trace), Observed::TraceDetail(detail)) => {
            compare_trace_detail(model.project(*project), *trace, detail.as_deref())
        }
        (Read::SpanDetail(project, trace, span_no), Observed::SpanDetail(ingest)) => {
            compare_span_detail(model.project(*project), *trace, *span_no, *ingest)
        }
        (Read::Scores(project), Observed::Scores(rows)) => {
            compare_scores(model.project(*project), rows)
        }
        (Read::ScoresById(project, ids), Observed::Scores(rows)) => {
            compare_scores_by_id(model.project(*project), ids, rows)
        }
        (Read::Latest(project), Observed::Latest(rows)) => {
            compare_latest(model.project(*project), rows)
        }
        (Read::Sessions(project), Observed::Sessions(rows)) => {
            compare_sessions(model.project(*project), rows)
        }
        (Read::Users(project), Observed::Users(rows)) => {
            compare_users(model.project(*project), rows)
        }
        (Read::Metrics(project, window), Observed::Kpis(kpis)) => {
            compare_kpis(model.project(*project), *window, kpis)
        }
        (Read::FilterTraceIds(project, window), Observed::TraceIds(ids)) => {
            compare_filter_ids(model.project(*project), *window, ids)
        }
        (Read::AlertValue(project, window, measure), Observed::Alert(value)) => {
            compare_alert(model.project(*project), *window, measure, *value)
        }
        _ => unreachable!("observe returns the kind of the read it runs: {read:?}"),
    }
}

/// Runs `read` on `h`'s storage and compares the result with `model`. Panics, with the op index
/// and the read, when the read fails or differs from the model.
pub(super) fn check_read(h: &Harness, model: &Model, read: &Read, index: usize) {
    let observed = h
        .block_on(observe(h.storage(), read))
        .unwrap_or_else(|error| panic!("op {index}, {read:?}: the read failed: {error}"));
    check_observed(model, read, &observed, index);
}

/// [`compare`], panicking with the op index and the read on a difference.
pub(super) fn check_observed(model: &Model, read: &Read, observed: &Observed, index: usize) {
    if let Err(diff) = compare(model, read, observed, Strictness::Exact) {
        panic!("op {index}, {read:?}: {diff}");
    }
}

/// After an op that touched one project: that project's spans, trace list and scores.
pub(super) fn check_quick(h: &Harness, model: &Model, project: P, index: usize) {
    for read in [
        Read::Spans(project),
        Read::Traces(project),
        Read::Scores(project),
    ] {
        check_read(h, model, &read, index);
    }
}

/// After Restart, Crash, LossyCrash and Settle, and at the end: both projects, the reads of
/// `check_quick`, plus the sampled details, the score reads by id and latest, the deletion
/// statuses, and the physical audit of the object store.
pub(super) fn check_full(h: &Harness, model: &mut Model, index: usize) {
    check_deletions(h, model, index);
    match h.chaos() {
        None => check_no_orphans(h, index),
        Some(_) => {
            check_never_listed(h, index);
            check_no_leaked_uploads(h, index);
        }
    }
    let model: &Model = model;
    for project in model.projects() {
        check_quick(h, model, project, index);
        check_paging(h, project, index);
        check_samples(h, model, project, index);
        check_score_reads(h, model, project, index);
        check_aggregates(h, model, project, index);
    }
}

/// Every object under `objects/` is named by a catalog row, so no crash left an object the
/// catalog does not know (R2.3). Obsolete files keep their rows until GC deletes them.
fn check_no_orphans(h: &Harness, index: usize) {
    let orphans = h.block_on(orphan_objects(h.dir()));
    assert!(
        orphans.is_empty(),
        "op {index}: objects with no catalog row: {orphans:?}"
    );
}

/// S3 mode: storage lists the bucket only in the audits, and never copies in it (spec 03 §5).
/// The audits' own lists are counted in `Harness::audit_lists`.
fn check_never_listed(h: &Harness, index: usize) {
    let Some(chaos) = h.chaos() else {
        return;
    };
    assert_eq!(
        chaos.count(Call::List),
        h.audit_lists,
        "op {index}: the store was listed outside the audit"
    );
    assert_eq!(
        chaos.count(Call::Copy),
        0,
        "op {index}: the store was copied"
    );
}

/// S3 mode (R4.2, R4.3): storage left no multipart upload open that it could have closed. An
/// upload that only a lifecycle rule removes (a failed abort, a lost create answer, a cancelled
/// life) is abandoned, not leaked, and is counted in the statistics instead.
pub(super) fn check_no_leaked_uploads(h: &Harness, index: usize) {
    let Some(chaos) = h.chaos() else {
        return;
    };
    let leaked = chaos.leaked_uploads();
    assert!(
        leaked.is_empty(),
        "op {index}: multipart uploads storage left open: {leaked:?}"
    );
}

/// Every registered deletion of both projects reads `Pending` or `Applied`. Once one reads
/// `Applied` it never reads `Pending` again (spec 03 §7). Dropped projects' deletions are
/// checked too: a drop marks them applied.
pub(super) fn check_deletions(h: &Harness, model: &mut Model, index: usize) {
    for project in model.projects() {
        for deletion in &mut model.project_mut(project).deletions {
            let status = h
                .block_on(h.storage().deletion_status(deletion.id))
                .expect("deletion_status of a registered deletion");
            match status {
                DeletionStatus::Applied { .. } => deletion.applied_seen = true,
                DeletionStatus::Pending => assert!(
                    !deletion.applied_seen,
                    "op {index}, project {project:?}: deletion {} reads Pending after it was Applied",
                    deletion.id
                ),
            }
        }
    }
}

/// Right after `DropB`: every deletion of the dropped project reads `Applied` at once (spec 03 §7,
/// the drop's second step).
pub(super) fn check_dropped(h: &Harness, model: &Model, index: usize) {
    for deletion in &model.b.deletions {
        let status = h
            .block_on(h.storage().deletion_status(deletion.id))
            .expect("deletion_status of a deletion of the dropped project");
        assert!(
            matches!(status, DeletionStatus::Applied { .. }),
            "op {index}, project {:?}: deletion {} of the dropped project reads {status:?}, expected Applied",
            model.b.project,
            deletion.id
        );
    }
}

/// Reads in small pages return exactly the rows of one page, in the same order. All six reads
/// (spans, traces and scores, each in pages and in one page) run on one snapshot, so a background
/// compaction between them cannot change what they see and the equality is exact.
fn check_paging(h: &Harness, project: P, index: usize) {
    let storage = h.storage();
    let ctx = h
        .block_on(context(storage, project))
        .expect("query context for the paging check");
    let paged = h
        .block_on(observe_spans(&ctx, 7))
        .expect("span_list in pages of 7");
    let single = h
        .block_on(observe_spans(&ctx, MAX_PAGE))
        .expect("span_list in one page");
    assert_eq!(
        paged, single,
        "op {index}, project {project:?}: span rows read in pages of 7 differ from one page"
    );
    let sort_key = |row: &(i64, String, String)| (row.0, row.2.clone(), row.1.clone());
    assert!(
        single
            .windows(2)
            .all(|pair| sort_key(&pair[0]) <= sort_key(&pair[1])),
        "op {index}, project {project:?}: span rows are not in (start_time, span_id, trace_id) order"
    );
    let traces_paged = h
        .block_on(observe_traces(&ctx, ALL_ROWS, 2))
        .expect("trace_list in pages of 2");
    let traces_single = h
        .block_on(observe_traces(&ctx, ALL_ROWS, MAX_PAGE))
        .expect("trace_list in one page");
    assert_eq!(
        traces_paged, traces_single,
        "op {index}, project {project:?}: trace rows read in pages of 2 differ from one page"
    );
    // Session scores share one `created_at`, so pages of 2 cut through ties.
    let scores_paged = h
        .block_on(observe_scores(&ctx, 2))
        .expect("score_list in pages of 2");
    let scores_single = h
        .block_on(observe_scores(&ctx, MAX_PAGE))
        .expect("score_list in one page");
    assert_eq!(
        scores_paged, scores_single,
        "op {index}, project {project:?}: score rows read in pages of 2 differ from one page"
    );
}

/// The score reads of `check_full`: `scores_by_id` over every score key the lane's projects can
/// write (`score_key_space`), and `latest_scores`.
fn check_score_reads(h: &Harness, model: &Model, project: P, index: usize) {
    let ids: Vec<String> = score_key_space(model.projects())
        .iter()
        .map(|key| score_id(key).to_string())
        .collect();
    check_read(h, model, &Read::ScoresById(project, ids), index);
    check_read(h, model, &Read::Latest(project), index);
}

/// What `latest_scores` must return: the live score of each `(target_key, name)`. The model
/// has one score id per `(project, target, name)`, so each key has one candidate and the
/// check is exact.
fn expected_latest(state: &ProjectModel) -> Vec<LatestScore> {
    state
        .expected_scores()
        .into_iter()
        .map(|score| {
            let target_key = if score.target_kind == "trace" {
                score.trace_id.clone()
            } else {
                score.session_id.clone()
            }
            .expect("a live score has its target key");
            LatestScore {
                target_key,
                name: score.name,
                score_id: score.score_id,
                value_num: score.value_num,
            }
        })
        .collect()
}

/// Two sets of scores, compared by `score_id` with every field equal.
fn same_scores(
    observed: &[ExpectedScore],
    expected: &[ExpectedScore],
    what: &str,
) -> Result<(), String> {
    let mut observed: Vec<&ExpectedScore> = observed.iter().collect();
    let mut expected: Vec<&ExpectedScore> = expected.iter().collect();
    observed.sort_by(|a, b| a.score_id.cmp(&b.score_id));
    expected.sort_by(|a, b| a.score_id.cmp(&b.score_id));
    let observed_ids: Vec<&str> = observed.iter().map(|row| row.score_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.score_id.as_str()).collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "{what}: the score ids differ from the model's: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    for (want, got) in expected.iter().zip(&observed) {
        if let Some(problem) = score_mismatch(want, got) {
            return Err(format!(
                "{what}, score {}: {problem}\nexpected {want:?}\nobserved {got:?}",
                want.score_id
            ));
        }
    }
    Ok(())
}

/// What differs between an expected and an observed score row, or `None`.
fn score_mismatch(expected: &ExpectedScore, observed: &ExpectedScore) -> Option<String> {
    let mut differing: Vec<&str> = Vec::new();
    let mut compare = |name: &'static str, same: bool| {
        if !same {
            differing.push(name);
        }
    };
    compare("name", expected.name == observed.name);
    compare("target_kind", expected.target_kind == observed.target_kind);
    compare("trace_id", expected.trace_id == observed.trace_id);
    compare("session_id", expected.session_id == observed.session_id);
    // Versions are small integers, so the values are exact.
    compare("value_num", expected.value_num == observed.value_num);
    compare("user_id", expected.user_id == observed.user_id);
    compare("created_at", expected.created_at == observed.created_at);
    (!differing.is_empty()).then(|| format!("differing fields: {differing:?}"))
}

/// `span_list` shows each visible span key: the keys read equal the visible keys. Each key is
/// listed once, or, under `Strictness::Concurrent`, up to once per visible copy (a copy that a
/// flush or compaction has not merged yet is listed too, R6.5 § The rule).
fn compare_span_keys(
    state: &ProjectModel,
    observed: &[(i64, String, String)],
    strictness: Strictness,
) -> Result<(), String> {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (_, trace, span) in observed {
        *counts.entry((trace.clone(), span.clone())).or_default() += 1;
    }
    let expected: BTreeMap<(String, String), usize> = state
        .visible_spans()
        .into_iter()
        .map(|((trace, span_no), ingests)| {
            (
                (
                    hex(trace_id(trace).as_bytes()),
                    hex(span_id(span_no).as_bytes()),
                ),
                ingests.len(),
            )
        })
        .collect();
    let observed_keys: Vec<&(String, String)> = counts.keys().collect();
    let expected_keys: Vec<&(String, String)> = expected.keys().collect();
    if observed_keys != expected_keys {
        return Err(format!(
            "the span keys read differ from the model's visible keys: observed {observed_keys:?}, expected {expected_keys:?}"
        ));
    }
    for (key, count) in &counts {
        let copies = expected[key];
        let most = match strictness {
            Strictness::Exact => 1,
            Strictness::Concurrent => copies,
        };
        if *count < 1 || *count > most {
            return Err(format!(
                "span {key:?} is listed {count} times, the model has {copies} visible copies"
            ));
        }
    }
    Ok(())
}

/// The `traces` list has the expected trace ids in `start_time DESC, trace_id DESC` order,
/// and each row matches its expected row.
fn compare_traces(state: &ProjectModel, observed: &[ExpectedTrace]) -> Result<(), String> {
    let visible = state.visible_spans();
    let mut expected = state.expected_traces();
    expected.sort_by(|a, b| (b.start_time, &b.trace_id).cmp(&(a.start_time, &a.trace_id)));

    let observed_ids: Vec<&str> = observed.iter().map(|row| row.trace_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.trace_id.as_str()).collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "trace ids or their order differ: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    for (want, got) in expected.iter().zip(observed) {
        let trace = trace_number(&want.trace_id);
        if let Some(problem) = trace_mismatch(
            want,
            got,
            state.unsettled(trace),
            copies_of(&visible, trace),
        ) {
            return Err(format!(
                "trace {trace}: {problem}\nexpected {want:?}\nobserved {got:?}"
            ));
        }
    }
    Ok(())
}

/// The score list: its rows in `created_at DESC, score_id DESC` order, and each row as the
/// model has it.
fn compare_scores(state: &ProjectModel, observed: &[ExpectedScore]) -> Result<(), String> {
    let mut ordered = state.expected_scores();
    ordered.sort_by(|a, b| (b.created_at, &b.score_id).cmp(&(a.created_at, &a.score_id)));
    let observed_ids: Vec<&str> = observed.iter().map(|row| row.score_id.as_str()).collect();
    let expected_ids: Vec<&str> = ordered.iter().map(|row| row.score_id.as_str()).collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "score rows are not in created_at DESC, score_id DESC order: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    same_scores(observed, &state.expected_scores(), "the score list")
}

/// `scores_by_id` for `ids`: the live scores of the project among them. A score the model has
/// that the ids do not name is not expected, so a score written after the ids were chosen
/// never counts as missing.
fn compare_scores_by_id(
    state: &ProjectModel,
    ids: &[String],
    observed: &[ExpectedScore],
) -> Result<(), String> {
    let requested: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let expected: Vec<ExpectedScore> = state
        .expected_scores()
        .into_iter()
        .filter(|score| requested.contains(score.score_id.as_str()))
        .collect();
    same_scores(observed, &expected, "scores_by_id over the requested ids")
}

/// `latest_scores`: the live score of each `(target_key, name)`.
fn compare_latest(state: &ProjectModel, observed: &[LatestScore]) -> Result<(), String> {
    let mut observed = observed.to_vec();
    let mut expected = expected_latest(state);
    observed.sort_by(|a, b| (&a.target_key, &a.name).cmp(&(&b.target_key, &b.name)));
    expected.sort_by(|a, b| (&a.target_key, &a.name).cmp(&(&b.target_key, &b.name)));
    if observed != expected {
        return Err(format!(
            "latest_scores differ from the live scores: observed {observed:?}, expected {expected:?}"
        ));
    }
    Ok(())
}

/// The trace detail of `trace`: `None` for a trace the model does not expect, else its row and
/// its span ids.
fn compare_trace_detail(
    state: &ProjectModel,
    trace: u8,
    observed: Option<&(ExpectedTrace, Vec<String>)>,
) -> Result<(), String> {
    let id = hex(trace_id(trace).as_bytes());
    let want = state
        .expected_traces()
        .into_iter()
        .find(|row| row.trace_id == id);
    match (want, observed) {
        (None, None) => Ok(()),
        (None, Some((row, _))) => Err(format!(
            "trace {trace} is not expected but has a detail: {row:?}"
        )),
        (Some(want), None) => Err(format!(
            "trace {trace} is expected but has no detail\nexpected {want:?}"
        )),
        (Some(want), Some((row, span_ids))) => {
            let visible = state.visible_spans();
            if let Some(problem) = trace_mismatch(
                &want,
                row,
                state.unsettled(trace),
                copies_of(&visible, trace),
            ) {
                return Err(format!(
                    "trace {trace} detail: {problem}\nexpected {want:?}\nobserved {row:?}"
                ));
            }
            check_span_ids(span_ids, &visible, trace)
        }
    }
}

/// The span detail of span `span_no` of `trace`: the ingest time of its newest visible copy, or
/// `None` when no copy is visible.
fn compare_span_detail(
    state: &ProjectModel,
    trace: u8,
    span_no: u8,
    observed: Option<i64>,
) -> Result<(), String> {
    let newest = state
        .visible_spans()
        .get(&(trace, span_no))
        .and_then(|ingests| ingests.iter().copied().max());
    if observed != newest {
        return Err(format!(
            "span_detail of {:?} is {observed:?}, expected the newest visible copy {newest:?}",
            (trace, span_no)
        ));
    }
    Ok(())
}

/// The span ids of a trace detail equal its visible keys, each at least once and at most once
/// per visible copy.
fn check_span_ids(observed: &[String], visible: &Visible, trace: u8) -> Result<(), String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for id in observed {
        *seen.entry(id.clone()).or_default() += 1;
    }
    let expected: BTreeMap<String, usize> = visible
        .iter()
        .filter(|((key_trace, _), _)| *key_trace == trace)
        .map(|((_, span_no), ingests)| (hex(span_id(*span_no).as_bytes()), ingests.len()))
        .collect();
    if seen.keys().collect::<Vec<_>>() != expected.keys().collect::<Vec<_>>() {
        return Err(format!(
            "trace {trace}: the detail's span ids differ from the visible keys"
        ));
    }
    for (id, count) in &seen {
        let copies = expected[id];
        if *count < 1 || *count > copies {
            return Err(format!(
                "trace {trace}: span {id} is read {count} times, the model has {copies} visible copies"
            ));
        }
    }
    Ok(())
}

/// The sampled reads of `check_full`: 3 span keys and 3 traces per project, the first, middle
/// and last of each, plus one miss of each kind when there is one.
fn check_samples(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    let visible = state.visible_spans();
    let keys: Vec<(u8, u8)> = visible.keys().copied().collect();
    // The keys with two or more visible copies: the only ones where `span_detail` picks
    // between duplicates.
    let duplicated: Vec<(u8, u8)> = visible
        .iter()
        .filter(|(_, ingests)| ingests.len() >= 2)
        .map(|(key, _)| *key)
        .collect();
    let mut reads: Vec<Read> = Vec::new();
    for (trace, span_no) in picks(&keys).into_iter().chain(picks(&duplicated)) {
        reads.push(Read::SpanDetail(project, trace, span_no));
    }
    let missing_key = (0..TRACES)
        .flat_map(|trace| (0..SPANS).map(move |span_no| (trace, span_no)))
        .find(|key| !visible.contains_key(key));
    if let Some((trace, span_no)) = missing_key {
        reads.push(Read::SpanDetail(project, trace, span_no));
    }

    let expected = state.expected_traces();
    let positions: Vec<usize> = (0..expected.len()).collect();
    for position in picks(&positions) {
        reads.push(Read::TraceDetail(
            project,
            trace_number(&expected[position].trace_id),
        ));
    }
    let expected_numbers: BTreeSet<u8> = expected
        .iter()
        .map(|row| trace_number(&row.trace_id))
        .collect();
    if let Some(absent) = (0..TRACES).find(|trace| !expected_numbers.contains(trace)) {
        reads.push(Read::TraceDetail(project, absent));
    }
    for read in &reads {
        check_read(h, model, read, index);
    }
}

/// The first, middle and last item, each once.
fn picks<T: Copy>(items: &[T]) -> Vec<T> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut indices = vec![0, items.len() / 2, items.len() - 1];
    indices.dedup();
    indices.into_iter().map(|index| items[index]).collect()
}

/// The trace number whose id is `id` (hex). Only the model's `TRACES` numbers exist.
fn trace_number(id: &str) -> u8 {
    (0..TRACES)
        .find(|&trace| hex(trace_id(trace).as_bytes()) == id)
        .expect("a trace id the model knows")
}

/// The visible copies of all of `trace`'s keys.
fn copies_of(visible: &Visible, trace: u8) -> i64 {
    visible
        .iter()
        .filter(|((key_trace, _), _)| *key_trace == trace)
        .map(|(_, ingests)| i64::try_from(ingests.len()).expect("a few copies per key"))
        .sum()
}

/// What differs between an expected and an observed trace row, or `None`.
///
/// An unsettled trace (unmerged re-sends, spec 03 §6 *Duplicates*) has its counts between the
/// expected value and the value with every visible copy counted. Its token and cost sums are
/// at least the expected ones, and `None` only where the expected sum is `None`: a duplicate
/// adds to a sum, never removes from it. `copies` is the trace's visible copies.
fn trace_mismatch(
    expected: &ExpectedTrace,
    observed: &ExpectedTrace,
    unsettled: bool,
    copies: i64,
) -> Option<String> {
    let mut differing: Vec<&str> = Vec::new();
    let mut compare = |name: &'static str, same: bool| {
        if !same {
            differing.push(name);
        }
    };
    compare("trace_id", expected.trace_id == observed.trace_id);
    compare("start_time", expected.start_time == observed.start_time);
    compare("end_time", expected.end_time == observed.end_time);
    compare("name", expected.name == observed.name);
    compare(
        "root_span_id",
        expected.root_span_id == observed.root_span_id,
    );
    compare("has_root", expected.has_root == observed.has_root);
    compare("status", expected.status == observed.status);
    compare("session_id", expected.session_id == observed.session_id);
    compare("user_id", expected.user_id == observed.user_id);
    compare("environment", expected.environment == observed.environment);
    compare("tags", expected.tags == observed.tags);
    compare("models", expected.models == observed.models);
    compare("prompts", expected.prompts == observed.prompts);
    if unsettled {
        let extra = copies - expected.span_count;
        for (name, want, got) in [
            ("span_count", expected.span_count, observed.span_count),
            ("error_count", expected.error_count, observed.error_count),
            ("llm_count", expected.llm_count, observed.llm_count),
        ] {
            compare(name, (want..=want + extra).contains(&got));
        }
        let tokens_bounded = match (expected.total_tokens, observed.total_tokens) {
            (None, None) => true,
            (Some(want), Some(got)) => got >= want,
            _ => false,
        };
        compare("total_tokens", tokens_bounded);
        let cost_bounded = match (expected.cost_total, observed.cost_total) {
            (None, None) => true,
            (Some(want), Some(got)) => got >= want - 1e-9,
            _ => false,
        };
        compare("cost_total", cost_bounded);
    } else {
        compare("span_count", expected.span_count == observed.span_count);
        compare("error_count", expected.error_count == observed.error_count);
        compare("llm_count", expected.llm_count == observed.llm_count);
        compare(
            "total_tokens",
            expected.total_tokens == observed.total_tokens,
        );
        let cost_same = match (expected.cost_total, observed.cost_total) {
            (None, None) => true,
            (Some(want), Some(got)) => (want - got).abs() <= 1e-9,
            _ => false,
        };
        compare("cost_total", cost_same);
    }
    (!differing.is_empty()).then(|| format!("differing fields: {differing:?}"))
}

/// The days `from..to` after D, as a window of nanoseconds.
pub(super) fn days(from: u8, to: u8) -> (i64, i64) {
    (midnight(day(from)), midnight(day(to)))
}

/// What the aggregate checks need from one project's visible copies.
pub(super) struct Copies {
    visible: Visible,
}

impl Copies {
    pub(super) fn of(state: &ProjectModel) -> Self {
        Self {
            visible: state.visible_spans(),
        }
    }

    /// The copies of `row`'s trace beyond one per visible key: what its unmerged re-sends add.
    fn extra(&self, row: &ExpectedTrace) -> i64 {
        copies_of(&self.visible, trace_number(&row.trace_id)) - row.span_count
    }

    /// A group is settled when none of its traces has extra copies, so its sums are exact.
    fn group_settled(&self, rows: &[&ExpectedTrace]) -> bool {
        rows.iter().all(|row| self.extra(row) == 0)
    }
}

/// The sums of one group of traces, from the expected rows.
#[derive(Debug)]
struct Sums {
    traces: i64,
    error_count: i64,
    /// The copies beyond one per key, summed over the group: an unsettled error sum may exceed
    /// the expected one by at most this much (each extra copy adds at most one error).
    extra_errors: i64,
    total_tokens: Option<i64>,
    cost_total: Option<f64>,
    /// No trace of the group has extra copies: its sums are exact.
    settled: bool,
}

impl Sums {
    fn of(rows: &[&ExpectedTrace], copies: &Copies) -> Self {
        Self {
            traces: i64::try_from(rows.len()).expect("a model has few traces"),
            error_count: rows.iter().map(|row| row.error_count).sum(),
            extra_errors: rows.iter().map(|row| copies.extra(row)).sum(),
            total_tokens: rows
                .iter()
                .filter_map(|row| row.total_tokens)
                .reduce(|total, value| total + value),
            cost_total: rows
                .iter()
                .filter_map(|row| row.cost_total)
                .reduce(|total, value| total + value),
            settled: copies.group_settled(rows),
        }
    }
}

/// The window `from..to` one span long before `window`: the one `metrics` compares against.
fn previous_window((from, to): (i64, i64)) -> (i64, i64) {
    (from - (to - from), from)
}

/// The aggregate reads of `check_full`, per project: the trace lists, sessions and users, the
/// metrics KPIs and the filter deletion scope over days D..D+4 and D+2..D+4, and the alert
/// values over days D..D+4 and D+1..D+2.
///
/// Set-valued parts are exact even when a trace is unsettled. A duplicate adds a copy of a key,
/// never a trace, and copies of a key share their content, so the traces, their merged start
/// times, `error_count > 0`, and the sessions and users they belong to are the same as with
/// one copy each. A sum is exact when no trace in its group has extra copies. Otherwise a
/// duplicate can only add: a token or cost sum is at least the expected one, and an error sum
/// is at most the extra copies above it.
fn check_aggregates(h: &Harness, model: &Model, project: P, index: usize) {
    for read in [
        Read::Windowed(project),
        Read::NoEval(project),
        Read::Sessions(project),
        Read::Users(project),
    ] {
        check_read(h, model, &read, index);
    }
    // Days D..D+4 has every trace; D+2..D+4 leaves the previous window, D..D+2, non-empty.
    for span in [(0, 4), (2, 4)] {
        let window = days(span.0, span.1);
        check_read(h, model, &Read::Metrics(project, window), index);
        check_read(h, model, &Read::FilterTraceIds(project, window), index);
    }
    // Days D+1..D+2 holds a class-2 trace whose score is in the window but whose trace starts
    // after it, so the semi-join excludes it.
    let mut measures = vec![AlertMeasure::ErrorRate, AlertMeasure::Cost];
    measures.extend((0..3).map(|k| AlertMeasure::ScoreAvg {
        name: format!("n{k}"),
    }));
    for span in [(0, 4), (1, 2)] {
        let window = days(span.0, span.1);
        for measure in &measures {
            let read = Read::AlertValue(project, window, measure.clone());
            check_read(h, model, &read, index);
        }
    }
}

/// The trace rows `observed` against `expected`, both of one trace list: the same trace ids, each
/// once, then each row as `trace_mismatch` compares it.
fn compare_trace_rows(
    state: &ProjectModel,
    observed: &[ExpectedTrace],
    expected: &[ExpectedTrace],
    what: &str,
) -> Result<(), String> {
    let copies = Copies::of(state);
    let mut observed: Vec<&ExpectedTrace> = observed.iter().collect();
    let mut expected: Vec<&ExpectedTrace> = expected.iter().collect();
    observed.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));
    expected.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));
    let observed_ids: Vec<&str> = observed.iter().map(|row| row.trace_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.trace_id.as_str()).collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "{what}: the trace ids differ from the model's: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    for (want, got) in expected.iter().zip(&observed) {
        let trace = trace_number(&want.trace_id);
        if let Some(problem) = trace_mismatch(
            want,
            got,
            state.unsettled(trace),
            copies_of(&copies.visible, trace),
        ) {
            return Err(format!(
                "{what}, trace {trace}: {problem}\nexpected {want:?}\nobserved {got:?}"
            ));
        }
    }
    Ok(())
}

/// `rows` keyed by `key`, which must be unique.
fn keyed<T>(
    rows: impl IntoIterator<Item = (String, T)>,
    what: &str,
) -> Result<BTreeMap<String, T>, String> {
    let mut map = BTreeMap::new();
    for (key, row) in rows {
        if map.insert(key.clone(), row).is_some() {
            return Err(format!("{what}: {key} is listed twice"));
        }
    }
    Ok(map)
}

/// Floats compare with `1e-9`. A sum that may be unsettled is at least the expected one.
fn float_sum_matches(want: Option<f64>, got: Option<f64>, settled: bool) -> bool {
    match (want, got) {
        (None, None) => true,
        (Some(want), Some(got)) => {
            if settled {
                (want - got).abs() <= 1e-9
            } else {
                got >= want - 1e-9
            }
        }
        _ => false,
    }
}

/// The sums of a group that differ from the observed ones, or `None`.
fn sums_mismatch(
    want: &Sums,
    errors: i64,
    tokens: Option<i64>,
    cost: Option<f64>,
) -> Option<String> {
    let mut differing: Vec<&str> = Vec::new();
    let errors_ok = if want.settled {
        errors == want.error_count
    } else {
        (want.error_count..=want.error_count + want.extra_errors).contains(&errors)
    };
    if !errors_ok {
        differing.push("error_count");
    }
    let tokens_ok = match (want.total_tokens, tokens) {
        (None, None) => true,
        (Some(expected), Some(got)) => {
            if want.settled {
                expected == got
            } else {
                got >= expected
            }
        }
        _ => false,
    };
    if !tokens_ok {
        differing.push("total_tokens");
    }
    if !float_sum_matches(want.cost_total, cost, want.settled) {
        differing.push("cost_total");
    }
    (!differing.is_empty()).then(|| format!("differing sums {differing:?}, expected {want:?}"))
}

/// `session_list` over all traces: the same session ids, the trace counts, and the sums.
fn compare_sessions(state: &ProjectModel, observed: &[SessionRow]) -> Result<(), String> {
    let copies = Copies::of(state);
    let rows = state.expected_traces();
    let mut sessions: BTreeMap<String, Vec<&ExpectedTrace>> = BTreeMap::new();
    for row in &rows {
        if let Some(session) = &row.session_id {
            sessions.entry(session.clone()).or_default().push(row);
        }
    }
    let observed = keyed(
        observed.iter().map(|row| (row.session_id.clone(), row)),
        "the session list",
    )?;
    let observed_ids: Vec<&String> = observed.keys().collect();
    let expected_ids: Vec<&String> = sessions.keys().collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "the session ids differ from the model's: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    for (session, members) in &sessions {
        let got = observed[session];
        let want = Sums::of(members, &copies);
        if got.trace_count != want.traces {
            return Err(format!(
                "session {session} trace_count is {}, expected {}",
                got.trace_count, want.traces
            ));
        }
        if let Some(problem) =
            sums_mismatch(&want, got.error_count, got.total_tokens, got.cost_total)
        {
            return Err(format!("session {session}: {problem}\nobserved {got:?}"));
        }
    }
    Ok(())
}

/// `user_list` over all traces: the same user ids, and each user's trace count.
fn compare_users(state: &ProjectModel, observed: &[(String, i64)]) -> Result<(), String> {
    let rows = state.expected_traces();
    let mut users: BTreeMap<String, Vec<&ExpectedTrace>> = BTreeMap::new();
    for row in &rows {
        if let Some(user) = &row.user_id {
            users.entry(user.clone()).or_default().push(row);
        }
    }
    let observed = keyed(
        observed.iter().map(|(user, count)| (user.clone(), *count)),
        "the user list",
    )?;
    let observed_ids: Vec<&String> = observed.keys().collect();
    let expected_ids: Vec<&String> = users.keys().collect();
    if observed_ids != expected_ids {
        return Err(format!(
            "the user ids differ from the model's: observed {observed_ids:?}, expected {expected_ids:?}"
        ));
    }
    for (user, members) in &users {
        let count = i64::try_from(members.len()).expect("a model has few traces");
        if observed[user] != count {
            return Err(format!(
                "user {user} trace_count is {}, expected {count}",
                observed[user]
            ));
        }
    }
    Ok(())
}

/// One KPI, which must be listed once.
fn kpi(kpis: &[Kpi], name: KpiName) -> Result<Kpi, String> {
    let mut found = kpis.iter().filter(|kpi| kpi.name == name);
    let kpi = found
        .next()
        .ok_or_else(|| format!("the metrics have no {name:?} KPI"))?;
    if found.next().is_some() {
        return Err(format!("the metrics list {name:?} twice"));
    }
    Ok(*kpi)
}

/// The KPIs `window` gives, as `metrics` computes them: each KPI's expected value and whether it
/// is exact. Counts and error shares are set-valued, so they are always exact.
fn expected_kpis(
    state: &ProjectModel,
    copies: &Copies,
    window: (i64, i64),
) -> Vec<(KpiName, Option<f64>, bool)> {
    let rows = state.traces_in(Some(window), false);
    let refs: Vec<&ExpectedTrace> = rows.iter().collect();
    let sums = Sums::of(&refs, copies);
    let errors = i64::try_from(rows.iter().filter(|row| row.error_count > 0).count())
        .expect("a model has few traces");
    vec![
        (
            KpiName::Traces,
            (sums.traces > 0).then_some(sums.traces as f64),
            true,
        ),
        (
            KpiName::ErrorRate,
            (sums.traces > 0).then(|| errors as f64 / sums.traces as f64),
            true,
        ),
        (KpiName::CostTotal, sums.cost_total, sums.settled),
        (
            KpiName::TotalTokens,
            sums.total_tokens.map(|total| total as f64),
            sums.settled,
        ),
    ]
}

/// The KPIs `kpis` read over `window` against the model, and the same over the previous window.
/// The percentile is not compared (approximate); it must only be listed.
fn compare_kpis(state: &ProjectModel, window: (i64, i64), kpis: &[Kpi]) -> Result<(), String> {
    let copies = Copies::of(state);
    let current = expected_kpis(state, &copies, window);
    let previous = expected_kpis(state, &copies, previous_window(window));
    for ((name, want, exact), (_, want_previous, previous_exact)) in current.iter().zip(&previous) {
        let got = kpi(kpis, *name)?;
        if !float_sum_matches(*want, got.value, *exact) {
            return Err(format!(
                "the {name:?} KPI is {:?}, expected {want:?}",
                got.value
            ));
        }
        if !float_sum_matches(*want_previous, got.previous, *previous_exact) {
            return Err(format!(
                "the {name:?} KPI's previous value is {:?}, expected {want_previous:?}",
                got.previous
            ));
        }
    }
    kpi(kpis, KpiName::LatencyP95)?;
    Ok(())
}

/// `filter_trace_ids` over `window`: the traces of the window's trace list, evaluation excluded.
fn compare_filter_ids(
    state: &ProjectModel,
    window: (i64, i64),
    observed: &[String],
) -> Result<(), String> {
    let mut want_ids: Vec<String> = state
        .traces_in(Some(window), false)
        .into_iter()
        .map(|row| row.trace_id)
        .collect();
    want_ids.sort();
    if observed != want_ids.as_slice() {
        return Err(format!(
            "filter_trace_ids differs from the trace list: observed {observed:?}, expected {want_ids:?}"
        ));
    }
    Ok(())
}

/// One `alert_value` over `window` for `measure`: the sample size and the value. The trace
/// measures are exact for their set-valued parts; the sums are as `compare_kpis` decides.
fn compare_alert(
    state: &ProjectModel,
    window: (i64, i64),
    measure: &AlertMeasure,
    value: AlertValue,
) -> Result<(), String> {
    match measure {
        AlertMeasure::ErrorRate => {
            let copies = Copies::of(state);
            let members = state.traces_in(Some(window), false);
            let refs: Vec<&ExpectedTrace> = members.iter().collect();
            let sums = Sums::of(&refs, &copies);
            let errors = i64::try_from(members.iter().filter(|row| row.error_count > 0).count())
                .expect("a model has few traces");
            let want_rate = (sums.traces > 0).then(|| errors as f64 / sums.traces as f64);
            if value.count != sums.traces {
                return Err(format!(
                    "the ErrorRate alert count is {}, expected {}",
                    value.count, sums.traces
                ));
            }
            if !float_sum_matches(want_rate, value.value, true) {
                return Err(format!(
                    "the ErrorRate alert is {:?}, expected {want_rate:?}",
                    value.value
                ));
            }
            Ok(())
        }
        AlertMeasure::Cost => {
            let copies = Copies::of(state);
            let members = state.traces_in(Some(window), false);
            let refs: Vec<&ExpectedTrace> = members.iter().collect();
            let sums = Sums::of(&refs, &copies);
            if value.count != sums.traces {
                return Err(format!(
                    "the Cost alert count is {}, expected {}",
                    value.count, sums.traces
                ));
            }
            if !float_sum_matches(
                Some(sums.cost_total.unwrap_or(0.0)),
                value.value,
                sums.settled,
            ) {
                return Err(format!(
                    "the Cost alert is {:?}, expected {:?}",
                    value.value, sums.cost_total
                ));
            }
            Ok(())
        }
        AlertMeasure::ScoreAvg { name } => {
            let (count, mean) = expected_score_avg(state, window, name);
            if value.count != count {
                return Err(format!(
                    "the ScoreAvg {name} alert count is {}, expected {count}",
                    value.count
                ));
            }
            // Exact even when a trace is unsettled. Duplicates add copies, not traces, so the
            // semi-join's trace set (and so the score set and its mean) is the same. That relies on
            // the shapes keeping `start_time`, `end_time` and `environment` identical across the
            // copies of a span: otherwise a copy could move a trace in or out of the window.
            if !float_sum_matches(mean, value.value, true) {
                return Err(format!(
                    "the ScoreAvg {name} alert is {:?}, expected {mean:?}",
                    value.value
                ));
            }
            Ok(())
        }
        other => unreachable!("the checks read only ErrorRate, Cost and ScoreAvg, not {other:?}"),
    }
}

/// `(count, mean)` of the live trace scores named `name` created in `window`, whose trace passes
/// the alert's semi-join (`reads/alerts.rs`, `trace_id_plan`): an expected evaluation-free trace
/// that ends at or after `from` and starts in `[from - 1 day, to)`.
fn expected_score_avg(
    state: &ProjectModel,
    (from, to): (i64, i64),
    name: &str,
) -> (i64, Option<f64>) {
    let traces: BTreeSet<String> = state
        .traces_in(Some((from - DAY_NS, to)), false)
        .into_iter()
        .filter(|row| row.end_time >= from)
        .map(|row| row.trace_id)
        .collect();
    let values: Vec<f64> = state
        .expected_scores()
        .into_iter()
        .filter(|score| {
            score.target_kind == "trace"
                && score.name == name
                && (from..to).contains(&score.created_at)
                && score
                    .trace_id
                    .as_ref()
                    .is_some_and(|id| traces.contains(id))
        })
        .map(|score| score.value_num)
        .collect();
    let count = i64::try_from(values.len()).expect("a model has few scores");
    let mean = (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64);
    (count, mean)
}

/// Polls `check` every 10 ms for up to 5 s, then fails with its last finding. For the state a
/// dropped `QueryContext` or a background task's last step may reach just after the passes.
fn eventually(what: &str, check: impl Fn() -> Result<(), String>) {
    let started = Instant::now();
    loop {
        match check() {
            Ok(()) => return,
            Err(found) if started.elapsed() >= Duration::from_secs(5) => {
                panic!("{what} after 5 s: {found}")
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// The object keys under `<data>/objects/tables`, relative to `objects/` (the catalog's paths).
/// Panics on a temp file, which a settled run must not leave (`.…tmp-…`).
fn local_table_keys(data: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, objects: &Path, keys: &mut BTreeSet<String>) {
        if !dir.exists() {
            // No table has been written yet.
            return;
        }
        for entry in std::fs::read_dir(dir).expect("read an objects directory") {
            let entry = entry.expect("read an objects directory entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, objects, keys);
                continue;
            }
            assert!(
                !entry.file_name().to_string_lossy().starts_with('.'),
                "a temp object is left on disk: {}",
                path.display()
            );
            let key = path
                .strip_prefix(objects)
                .expect("an object under objects/")
                .to_string_lossy()
                .replace('\\', "/");
            keys.insert(key);
        }
    }
    let objects = data.join("objects");
    let mut keys = BTreeSet::new();
    walk(&objects.join("tables"), &objects, &mut keys);
    keys
}

/// After `settle_concurrent`: nothing in flight, nothing leaked (R6.6). Checks 1-6 and 8 run
/// first, then check 7 restarts the harness, so it runs last.
pub(super) fn check_quiet_state(h: &mut Harness) {
    {
        let h: &Harness = h;
        let storage = h.storage();
        // 1. Memtables empty.
        let health = storage.health();
        assert_eq!(health.memtable_bytes, 0, "memtable bytes after settle");
        assert_eq!(
            storage.published_rows(Table::Spans),
            0,
            "spans in memtables"
        );
        assert_eq!(
            storage.published_rows(Table::Scores),
            0,
            "scores in memtables"
        );
        // 2. No compaction claims.
        assert_eq!(
            storage.compaction_claims(),
            0,
            "compaction claims after settle"
        );
        // 3. No query memory held: each query pool returns its bytes on drop.
        eventually("query memory is still held", || {
            let used = storage.qe_runtime.admission.used();
            (used == 0)
                .then_some(())
                .ok_or_else(|| format!("{used} bytes of query memory are held"))
        });
        // 4. No disk reservation.
        assert_eq!(
            storage.disk().reserved(),
            0,
            "compaction disk reservations after settle"
        );
        // 5. No staged rows.
        eventually("staged rows are left", || {
            let staged = h
                .block_on(storage.staged_paths())
                .expect("list staged objects");
            staged
                .is_empty()
                .then_some(())
                .ok_or_else(|| format!("{} staged objects are left", staged.len()))
        });
        // 6. No orphan objects: the data objects are exactly the active catalog files.
        let catalog: BTreeSet<String> = [FileTable::Spans, FileTable::Traces, FileTable::Scores]
            .into_iter()
            .flat_map(|table| storage.catalog_paths(table))
            .collect();
        let stored: BTreeSet<String> = match h.chaos() {
            None => local_table_keys(h.dir()),
            Some(chaos) => h.block_on(async {
                use futures::TryStreamExt;
                let prefix = object_store::path::Path::from("tables");
                chaos
                    .inner()
                    .list(Some(&prefix))
                    .map_ok(|meta| meta.location.as_ref().to_owned())
                    .try_collect()
                    .await
                    .expect("list the data objects of the store")
            }),
        };
        let orphans: Vec<&String> = stored.difference(&catalog).collect();
        let missing: Vec<&String> = catalog.difference(&stored).collect();
        assert!(
            orphans.is_empty() && missing.is_empty(),
            "objects and catalog files differ after settle: {} orphan objects {orphans:?}, {} missing {missing:?}",
            orphans.len(),
            missing.len()
        );
        // 8. No quarantine: nothing is corrupt in this test.
        let pending = storage.verify_queue().take(usize::MAX);
        assert!(
            pending.is_empty(),
            "{} files wait for the verify pass after settle",
            pending.len()
        );
        let quarantined: i64 = h.block_on(async {
            use sqlx::ConnectOptions;
            let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(h.dir().join("catalog.db"))
                .read_only(true)
                .connect()
                .await
                .expect("open catalog.db read-only");
            sqlx::query_scalar("SELECT COUNT(*) FROM data_files WHERE state = 'quarantined'")
                .fetch_one(&mut connection)
                .await
                .expect("count the quarantined files")
        });
        assert_eq!(quarantined, 0, "healthy files are quarantined");
    }
    // 7. The WAL is fully flushed: a restart replays nothing, and each WAL directory holds at
    //    most the newest segment that truncation keeps and the one this life writes to.
    h.restart();
    let storage = h.storage();
    assert_eq!(
        storage.replayed(Table::Spans),
        0,
        "spans replayed at the restart"
    );
    assert_eq!(
        storage.replayed(Table::Scores),
        0,
        "scores replayed at the restart"
    );
    for table in [Table::Spans, Table::Scores] {
        let dir = h.dir().join("wal").join(table.name());
        let segments: Vec<String> = std::fs::read_dir(&dir)
            .expect("read a WAL directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| crate::wal::parse_segment_name(name).is_some())
            .collect();
        assert!(
            segments.len() <= 2,
            "{} WAL segments after a restart, expected at most 2: {segments:?}",
            segments.len()
        );
    }
}

/// After `Settle`, the files on disk hold what the model predicts and nothing more (plan § R1.7):
/// no row a deletion covers, no row before the retention watermark, no file of a dropped project,
/// one partial per trace (two for class 2), and no staged object left.
pub(super) fn check_settled_files(h: &Harness, models: &[&Model]) {
    let traces_by_id: BTreeMap<[u8; 16], u8> = (0..TRACES)
        .map(|trace| (*trace_id(trace).as_bytes(), trace))
        .collect();
    let keys_by_id: BTreeMap<String, ScoreKey> = models
        .iter()
        .flat_map(|model| score_key_space(model.projects()))
        .map(|key| (score_id(&key).to_string(), key))
        .collect();
    // Every catalogued file belongs to a lane: the per-project checks below only see the files
    // of the lanes' projects.
    for table in [FileTable::Spans, FileTable::Traces, FileTable::Scores] {
        for file in h.storage().catalog_files(table) {
            assert!(
                h.foreign_projects().contains(&file.project)
                    || models
                        .iter()
                        .any(|model| model.projects().iter().any(|p| p.id() == file.project)),
                "file of project {} belongs to no lane",
                file.project
            );
        }
    }
    for model in models {
        for project in model.projects() {
            let state = model.project(project);
            check_settled_spans(h, state, project, &traces_by_id);
            check_settled_scores(h, state, project, &keys_by_id);
            check_settled_traces(h, state, project, &traces_by_id);
        }
    }
    let staged = h
        .block_on(h.storage().staged_paths())
        .expect("staged_paths after settle");
    assert!(staged.is_empty(), "objects staged after settle: {staged:?}");
}

/// The catalogued files of `table` that belong to `project`.
fn files_of(h: &Harness, table: FileTable, project: P) -> Vec<Arc<crate::catalog::FileMeta>> {
    h.storage()
        .catalog_files(table)
        .into_iter()
        .filter(|file| file.project == project.id())
        .collect()
}

fn check_settled_spans(
    h: &Harness,
    state: &ProjectModel,
    project: P,
    traces_by_id: &BTreeMap<[u8; 16], u8>,
) {
    for file in files_of(h, FileTable::Spans, project) {
        assert!(
            !state.dropped,
            "project {project:?} is dropped, but spans file {} is still catalogued",
            file.path
        );
        if let Some(keep) = state.keep_from {
            assert!(
                file.day >= keep,
                "spans file {} of day {} is before the retention watermark {keep}",
                file.path,
                file.day
            );
        }
        let batch = statam_rows(h.object_bytes(&file), h.pack_bytes(&file));
        let trace_bytes = trace_ids_of(&batch);
        let span_col = crate::reads::columns::require_col(&batch, spans::SPAN_ID)
            .expect("a spans file has a span_id column");
        let ingest_col = crate::reads::columns::require_col(&batch, spans::INGEST_TIME)
            .expect("a spans file has an ingest_time column");
        let mut keys = BTreeSet::new();
        for (row, bytes) in trace_bytes.iter().enumerate() {
            let trace = *traces_by_id
                .get(bytes)
                .expect("a stored span belongs to a trace of the model");
            let ingest =
                crate::reads::columns::time_at(&batch, ingest_col, row).expect("ingest_time");
            assert!(
                !state.covers_span(trace, ingest),
                "project {project:?}: spans file {} keeps trace {trace} written at {ingest}, \
                 which a deletion covers",
                file.path
            );
            let phys = crate::reads::columns::physical_row(&batch, row).expect("span row");
            let span_bytes = match span_col.get_scalar(phys).expect("span_id scalar") {
                ScalarValue::FixedBytes8(bytes) => bytes.to_vec(),
                other => panic!("unexpected span_id value: {other:?}"),
            };
            assert!(
                keys.insert((trace, span_bytes)),
                "project {project:?}: spans file {} holds a span of trace {trace} twice",
                file.path
            );
        }
    }
}

fn check_settled_scores(
    h: &Harness,
    state: &ProjectModel,
    project: P,
    keys_by_id: &BTreeMap<String, ScoreKey>,
) {
    for file in files_of(h, FileTable::Scores, project) {
        assert!(
            file.fully_merged,
            "scores file {} is not fully merged after settle",
            file.path
        );
        let batch = statam_rows(h.object_bytes(&file), h.pack_bytes(&file));
        let ids = crate::reads::columns::require_col(&batch, scores::SCORE_ID)
            .expect("a scores file has a score_id column");
        let deleted = crate::reads::columns::require_col(&batch, scores::DELETED)
            .expect("a scores file has a deleted column");
        let ingests = crate::reads::columns::require_col(&batch, scores::INGEST_TIME)
            .expect("a scores file has an ingest_time column");
        for row in 0..batch.num_rows() {
            let id = crate::reads::columns::string_at(&batch, ids, row)
                .expect("score_id")
                .expect("non-null score_id");
            let key = keys_by_id
                .get(&id)
                .expect("a stored score belongs to a key of the model");
            assert_eq!(
                key.project, project,
                "scores file {} of project {project:?} holds a score of {:?}",
                file.path, key.project
            );
            assert!(
                !crate::reads::columns::bool_at(&batch, deleted, row)
                    .expect("deleted")
                    .unwrap_or(false),
                "scores file {} keeps a deleted version after settle",
                file.path
            );
            let ingest = crate::reads::columns::time_at(&batch, ingests, row).expect("ingest_time");
            assert!(
                !state.covers_score(key, ingest),
                "project {project:?}: scores file {} keeps a score a deletion covers",
                file.path
            );
        }
    }
}

/// After `settle`, in S3 mode (R4.4): the store holds exactly the objects the catalog names (no
/// leak, no lost file), no catalog row is obsolete, and the read cache serves what the store
/// holds. The orphan audit is a dry run of N's `collect_orphans`, through the inner store, so it
/// sees neither faults nor the cache.
pub(super) fn check_s3_audit(h: &mut Harness) {
    let Some(chaos) = h.chaos().cloned() else {
        return;
    };
    let lists_before = chaos.count(Call::List);
    let counts = h.block_on(collect_orphans_dry_run(h.dir(), chaos.inner().as_ref()));
    h.audit_lists += chaos.count(Call::List) - lists_before;
    assert_eq!(
        counts.orphans, 0,
        "the store holds objects no catalog row names: a leak"
    );
    // B5's `collect_orphans` will bring the age filter, and then the unknown objects younger than
    // it are counted here as `recent_unknown`. Until then every unknown object is an orphan above.
    assert_eq!(
        counts.missing, 0,
        "the catalog names data files the store does not have: lost data"
    );
    assert_eq!(
        counts.missing_media, 0,
        "the catalog names media the store does not have"
    );
    let obsolete = h.block_on(obsolete_rows(h.dir()));
    assert_eq!(obsolete, 0, "the catalog has obsolete rows after settle");

    let cache = h
        .storage()
        .objects
        .read_cache()
        .expect("S3 mode runs with the read cache on");
    let files: usize = [FileTable::Spans, FileTable::Traces, FileTable::Scores]
        .into_iter()
        .map(|table| h.storage().catalog_files(table).len())
        .sum();
    let before = cache.stats();
    if files > 0 {
        assert!(
            before.blocks > 0,
            "the catalog has {files} files but the read cache holds no block"
        );
    }
    for table in [FileTable::Spans, FileTable::Traces, FileTable::Scores] {
        for file in h.storage().catalog_files(table) {
            // Bounded, with the catalog's size: an unbounded read bypasses the cache (read_cache.rs).
            let cached = h
                .block_on(
                    h.storage()
                        .objects
                        .get_range(&file.path, Some(0..file.size)),
                )
                .expect("read a file through the read cache");
            assert!(
                cached == h.object_bytes(&file),
                "the read cache serves other bytes than the store for {}",
                file.path
            );
        }
    }
    let after = cache.stats();
    if files > 0 {
        assert!(
            after.hits > before.hits,
            "reading {files} files through the read cache served no block from disk"
        );
    }
}

/// The merged `traces` rows of `project`, read through the traces provider as the server does:
/// one row per trace, with its partial count.
fn stored_partials(h: &Harness, project: P) -> Vec<(String, i32)> {
    h.block_on(async {
        let query = h
            .storage()
            .query(project.id(), crate::QueryOptions::new(None, None))
            .await
            .expect("query for the traces file check");
        let batches = query.collect_raw_traces().await.expect("collect traces_t");
        let mut rows = Vec::new();
        for batch in batches {
            let ids =
                crate::reads::columns::require_col(&batch, cetra_core::schema::traces::TRACE_ID)
                    .expect("traces trace_id");
            let partials = crate::reads::columns::require_col(
                &batch,
                cetra_core::schema::traces::PARTIAL_COUNT,
            )
            .expect("traces partial_count");
            for row in 0..batch.num_rows() {
                let phys = crate::reads::columns::physical_row(&batch, row).expect("trace row");
                let trace_id = match ids.get_scalar(phys).expect("trace_id scalar") {
                    ScalarValue::FixedBytes16(bytes) => hex(&bytes),
                    other => panic!("unexpected trace_id value: {other:?}"),
                };
                let partial = i32::try_from(
                    crate::reads::columns::i64_at(&batch, partials, row).expect("partial_count"),
                )
                .expect("partial_count fits in i32");
                rows.push((trace_id, partial));
            }
        }
        rows
    })
}

fn check_settled_traces(
    h: &Harness,
    state: &ProjectModel,
    project: P,
    traces_by_id: &BTreeMap<[u8; 16], u8>,
) {
    let expected: BTreeSet<String> = state
        .expected_traces()
        .into_iter()
        .map(|row| row.trace_id)
        .collect();
    let stored = stored_partials(h, project);
    let observed: BTreeSet<String> = stored.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        observed.len(),
        stored.len(),
        "project {project:?}: the traces provider returns a trace twice after settle"
    );
    assert_eq!(
        observed, expected,
        "project {project:?}: the traces provider after settle lists other traces than the model"
    );
    let trace_of_hex: BTreeMap<String, u8> = traces_by_id
        .iter()
        .map(|(bytes, trace)| (hex(bytes), *trace))
        .collect();
    for (id, partials) in &stored {
        let trace = trace_of_hex[id];
        if trace % 4 == 2 {
            // Class 2 crosses midnight: one partial per day once its anchor expired (spec 03
            // § Trace anchor, known limit).
            assert!(
                (1..=2).contains(partials),
                "project {project:?}: class-2 trace {trace} has {partials} partials, expected 1 or 2"
            );
        } else {
            assert_eq!(
                *partials, 1,
                "project {project:?}: trace {trace} has {partials} partials after settle, expected 1"
            );
        }
    }
}
