//! The checks after each op: storage's reads against the model's expected rows
//! (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! A failure panics with the op index, the project, and the expected and observed values.
//! proptest prints the shrunk op list.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arrow::array::AsArray;
use datafusion::execution::context::{SessionConfig, SessionContext};
use llmobs_core::schema::{scores, spans};

use super::harness::Harness;
use super::model::{ExpectedScore, ExpectedTrace, Model, ProjectModel};
use super::observe::{
    ALL_ROWS, LatestScore, SessionRow, observe_alert, observe_filter_trace_ids, observe_kpis,
    observe_latest, observe_scores, observe_scores_by_id, observe_sessions, observe_span_detail,
    observe_spans, observe_trace_detail, observe_traces, observe_users, window_of,
};
use super::ops::{SPANS, TRACES, score_key_space};
use super::shape::{DAY_NS, P, ScoreKey, day, hex, midnight, score_id, span_id, trace_id};
use crate::catalog::FileTable;
use crate::deletion::DeletionStatus;
use crate::reads::{AlertMeasure, Kpi, KpiName, ListScope, MAX_PAGE};
use crate::test_support::{
    Call, collect_orphans_dry_run, obsolete_rows, orphan_objects, parquet_rows, trace_ids_of,
};

/// Visible copies per span key, as `ProjectModel::visible_spans` returns them.
type Visible = BTreeMap<(u8, u8), Vec<i64>>;

/// After an op that touched one project: that project's spans, trace list and scores.
pub(super) fn check_quick(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    check_span_keys(h, state, project, index);
    check_traces(h, state, project, index);
    check_scores(h, state, project, index);
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
        let state = model.project(project);
        check_quick(h, model, project, index);
        check_paging(h, project, index);
        check_samples(h, state, project, index);
        check_score_reads(h, model, project, index);
        check_aggregates(h, state, project, index);
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
            .expect("deletion_status of a deletion of B");
        assert!(
            matches!(status, DeletionStatus::Applied { .. }),
            "op {index}, project B: deletion {} of the dropped project reads {status:?}, expected Applied",
            deletion.id
        );
    }
}

/// Reads in small pages return exactly the rows of one page, in the same order.
fn check_paging(h: &Harness, project: P, index: usize) {
    let paged = observe_spans(h, project, 7);
    let single = observe_spans(h, project, MAX_PAGE);
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
    assert_eq!(
        observe_traces(h, project, ALL_ROWS, 2),
        observe_traces(h, project, ALL_ROWS, MAX_PAGE),
        "op {index}, project {project:?}: trace rows read in pages of 2 differ from one page"
    );
    // Session scores share one `created_at`, so pages of 2 cut through ties.
    assert_eq!(
        observe_scores(h, project, 2),
        observe_scores(h, project, MAX_PAGE),
        "op {index}, project {project:?}: score rows read in pages of 2 differ from one page"
    );
}

/// The live scores of `project` are the model's live scores, each field equal, and the rows
/// come in `created_at DESC, score_id DESC` order as read (`reads/scores.rs`).
fn check_scores(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    let observed = observe_scores(h, project, MAX_PAGE);
    let mut ordered = state.expected_scores();
    ordered.sort_by(|a, b| (b.created_at, &b.score_id).cmp(&(a.created_at, &a.score_id)));
    let observed_ids: Vec<String> = observed.iter().map(|row| row.score_id.clone()).collect();
    let expected_ids: Vec<String> = ordered.iter().map(|row| row.score_id.clone()).collect();
    assert_eq!(
        observed_ids, expected_ids,
        "op {index}, project {project:?}: score rows are not in created_at DESC, score_id DESC order"
    );
    assert_same_scores(
        observed,
        state.expected_scores(),
        &format!("op {index}, project {project:?}: the score list"),
    );
}

/// `scores_by_id` over every score key the lane's projects can write (`score_key_space`) returns
/// exactly this project's live scores, so ids the model hides or never saw are tested too.
/// `latest_scores` returns the live score of each key.
fn check_score_reads(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    let ids: Vec<String> = score_key_space(model.projects())
        .iter()
        .map(|key| score_id(key).to_string())
        .collect();
    assert_same_scores(
        observe_scores_by_id(h, project, &ids),
        state.expected_scores(),
        &format!("op {index}, project {project:?}: scores_by_id over every written id"),
    );

    let mut observed = observe_latest(h, project);
    let mut expected = expected_latest(state);
    observed.sort_by(|a, b| (&a.target_key, &a.name).cmp(&(&b.target_key, &b.name)));
    expected.sort_by(|a, b| (&a.target_key, &a.name).cmp(&(&b.target_key, &b.name)));
    assert_eq!(
        observed, expected,
        "op {index}, project {project:?}: latest_scores differ from the live scores"
    );
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
pub(super) fn assert_same_scores(
    mut observed: Vec<ExpectedScore>,
    mut expected: Vec<ExpectedScore>,
    what: &str,
) {
    observed.sort_by(|a, b| a.score_id.cmp(&b.score_id));
    expected.sort_by(|a, b| a.score_id.cmp(&b.score_id));
    let observed_ids: Vec<&str> = observed.iter().map(|row| row.score_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.score_id.as_str()).collect();
    assert_eq!(
        observed_ids, expected_ids,
        "{what}: the score ids differ from the model's"
    );
    for (want, got) in expected.iter().zip(&observed) {
        if let Some(problem) = score_mismatch(want, got) {
            panic!(
                "{what}, score {}: {problem}\nexpected {want:?}\nobserved {got:?}",
                want.score_id
            );
        }
    }
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

/// `span_list` shows each visible span key exactly once: the keys read equal the visible keys,
/// and no key repeats.
fn check_span_keys(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    compare_span_keys(state, &observe_spans(h, project, MAX_PAGE), index, project);
}

/// The `span_list` rows `observed` (`(start_time, trace_id, span_id)`) against the model: the keys
/// are the visible keys, each listed once. `check_quick` and the `FaultyRead` op share it (R4.5).
pub(super) fn compare_span_keys(
    state: &ProjectModel,
    observed: &[(i64, String, String)],
    index: usize,
    project: P,
) {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (_, trace, span) in observed {
        *counts.entry((trace.clone(), span.clone())).or_default() += 1;
    }
    let observed = counts;
    let expected: BTreeSet<(String, String)> = state
        .visible_spans()
        .into_keys()
        .map(|(trace, span_no)| {
            (
                hex(trace_id(trace).as_bytes()),
                hex(span_id(span_no).as_bytes()),
            )
        })
        .collect();
    assert_eq!(
        observed.keys().cloned().collect::<BTreeSet<_>>(),
        expected,
        "op {index}, project {project:?}: the span keys read differ from the model's visible keys"
    );
    for (key, count) in &observed {
        assert_eq!(
            *count, 1,
            "op {index}, project {project:?}: span {key:?} is listed {count} times, expected once"
        );
    }
}

/// The `traces` list has the expected trace ids in `start_time DESC, trace_id DESC` order,
/// and each row matches its expected row.
fn check_traces(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    let observed = observe_traces(h, project, ALL_ROWS, MAX_PAGE);
    compare_traces(state, &observed, index, project);
}

/// The trace list `observed` against the model's expected traces of `project`: the ids in order,
/// then each row. `check_quick` and the `FaultyRead` op share it (R4.5).
pub(super) fn compare_traces(
    state: &ProjectModel,
    observed: &[ExpectedTrace],
    index: usize,
    project: P,
) {
    let visible = state.visible_spans();
    let mut expected = state.expected_traces();
    expected.sort_by(|a, b| (b.start_time, &b.trace_id).cmp(&(a.start_time, &a.trace_id)));

    let observed_ids: Vec<&str> = observed.iter().map(|row| row.trace_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.trace_id.as_str()).collect();
    assert_eq!(
        observed_ids, expected_ids,
        "op {index}, project {project:?}: trace ids or their order differ"
    );
    for (want, got) in expected.iter().zip(observed) {
        let trace = trace_number(&want.trace_id);
        if let Some(problem) = trace_mismatch(
            want,
            got,
            state.unsettled(trace),
            copies_of(&visible, trace),
        ) {
            panic!(
                "op {index}, project {project:?}, trace {trace}: {problem}\n\
                 expected {want:?}\nobserved {got:?}"
            );
        }
    }
}

/// Sampled details (`check_full` only): 3 span keys and 3 traces per project, the first,
/// middle and last of each, plus one miss of each kind when there is one.
fn check_samples(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    let visible = state.visible_spans();
    let keys: Vec<(u8, u8)> = visible.keys().copied().collect();
    // The keys with two or more visible copies: the only ones where `span_detail` picks
    // between duplicates.
    let duplicated: Vec<(u8, u8)> = visible
        .iter()
        .filter(|(_, ingests)| ingests.len() >= 2)
        .map(|(key, _)| *key)
        .collect();
    for key in picks(&keys).into_iter().chain(picks(&duplicated)) {
        let (trace, span_no) = key;
        let newest = visible[&key]
            .iter()
            .copied()
            .max()
            .expect("a visible key has a copy");
        assert_eq!(
            observe_span_detail(h, project, trace, span_no),
            Some(newest),
            "op {index}, project {project:?}: span_detail of {key:?} is not the newest visible copy"
        );
    }
    let missing_key = (0..TRACES)
        .flat_map(|trace| (0..SPANS).map(move |span_no| (trace, span_no)))
        .find(|key| !visible.contains_key(key));
    if let Some((trace, span_no)) = missing_key {
        assert!(
            observe_span_detail(h, project, trace, span_no).is_none(),
            "op {index}, project {project:?}: span_detail of the hidden key {:?} is not None",
            (trace, span_no)
        );
    }

    let expected = state.expected_traces();
    let positions: Vec<usize> = (0..expected.len()).collect();
    for position in picks(&positions) {
        let want = &expected[position];
        let trace = trace_number(&want.trace_id);
        let (row, span_ids) = observe_trace_detail(h, project, trace).unwrap_or_else(|| {
            panic!("op {index}, project {project:?}: trace {trace} is expected but has no detail")
        });
        if let Some(problem) = trace_mismatch(
            want,
            &row,
            state.unsettled(trace),
            copies_of(&visible, trace),
        ) {
            panic!(
                "op {index}, project {project:?}, trace {trace} detail: {problem}\n\
                 expected {want:?}\nobserved {row:?}"
            );
        }
        check_span_ids(&span_ids, &visible, trace, index, project);
    }

    let expected_numbers: BTreeSet<u8> = expected
        .iter()
        .map(|row| trace_number(&row.trace_id))
        .collect();
    if let Some(absent) = (0..TRACES).find(|trace| !expected_numbers.contains(trace)) {
        assert!(
            observe_trace_detail(h, project, absent).is_none(),
            "op {index}, project {project:?}: trace {absent} is not expected but has a detail"
        );
    }
}

/// The span ids of a trace detail equal its visible keys, each at least once and at most once
/// per visible copy.
fn check_span_ids(observed: &[String], visible: &Visible, trace: u8, index: usize, project: P) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for id in observed {
        *seen.entry(id.clone()).or_default() += 1;
    }
    let expected: BTreeMap<String, usize> = visible
        .iter()
        .filter(|((key_trace, _), _)| *key_trace == trace)
        .map(|((_, span_no), ingests)| (hex(span_id(*span_no).as_bytes()), ingests.len()))
        .collect();
    assert_eq!(
        seen.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "op {index}, project {project:?}, trace {trace}: the detail's span ids differ from the visible keys"
    );
    for (id, count) in &seen {
        let copies = expected[id];
        assert!(
            *count >= 1 && *count <= copies,
            "op {index}, project {project:?}, trace {trace}: span {id} is read {count} times, \
             the model has {copies} visible copies"
        );
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

/// Sessions, users, the windowed and evaluation-excluded trace lists, the metrics KPIs and their
/// previous values, the filter deletion scope and the alert values (`check_full` only), per
/// project.
///
/// Set-valued parts are exact even when a trace is unsettled. A duplicate adds a copy of a key,
/// never a trace, and copies of a key share their content, so the traces, their merged start
/// times, `error_count > 0`, and the sessions and users they belong to are the same as with
/// one copy each. A sum is exact when no trace in its group has extra copies. Otherwise a
/// duplicate can only add: a token or cost sum is at least the expected one, and an error sum
/// is at most the extra copies above it.
fn check_aggregates(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    let at = format!("op {index}, project {project:?}");
    let copies = Copies::of(state);
    check_trace_lists(h, state, &copies, project, &at);
    check_sessions_and_users(h, state, &copies, project, &at);
    // Days D..D+4 has every trace; D+2..D+4 leaves the previous window, D..D+2, non-empty.
    for span in [(0, 4), (2, 4)] {
        let label = format!("{at}: days D+{}..D+{}", span.0, span.1);
        let window = days(span.0, span.1);
        check_kpis(h, state, project, window, &label);
        check_filter_ids(h, state, project, window, &label);
    }
    // Days D+1..D+2 holds a class-2 trace whose score is in the window but whose trace starts
    // after it, so the semi-join excludes it.
    for span in [(0, 4), (1, 2)] {
        let label = format!("{at}: alerts over days D+{}..D+{}", span.0, span.1);
        check_alerts(h, state, &copies, project, days(span.0, span.1), &label);
    }
}

/// The trace list over days D+1..D+2 (merged `start_time`, evaluation traces kept) and the trace
/// list without evaluation traces.
fn check_trace_lists(h: &Harness, state: &ProjectModel, copies: &Copies, project: P, at: &str) {
    let window = days(1, 2);
    let windowed = observe_traces(
        h,
        project,
        ListScope {
            window: Some(window_of(window)),
            include_eval: true,
            include_experiments: true,
        },
        MAX_PAGE,
    );
    compare_trace_rows(
        state,
        copies,
        &windowed,
        &state.traces_in(Some(window), true),
        &format!("{at}: the trace list over days D+1..D+2"),
    );
    let kept = observe_traces(
        h,
        project,
        ListScope {
            window: None,
            include_eval: false,
            include_experiments: true,
        },
        MAX_PAGE,
    );
    compare_trace_rows(
        state,
        copies,
        &kept,
        &state.traces_in(None, false),
        &format!("{at}: the trace list without evaluation traces"),
    );
}

/// Two trace lists: the same trace ids, each once, then each row as `trace_mismatch` compares it.
fn compare_trace_rows(
    state: &ProjectModel,
    copies: &Copies,
    observed: &[ExpectedTrace],
    expected: &[ExpectedTrace],
    what: &str,
) {
    let mut observed: Vec<&ExpectedTrace> = observed.iter().collect();
    let mut expected: Vec<&ExpectedTrace> = expected.iter().collect();
    observed.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));
    expected.sort_by(|a, b| a.trace_id.cmp(&b.trace_id));
    let observed_ids: Vec<&str> = observed.iter().map(|row| row.trace_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.trace_id.as_str()).collect();
    assert_eq!(
        observed_ids, expected_ids,
        "{what}: the trace ids differ from the model's"
    );
    for (want, got) in expected.iter().zip(&observed) {
        let trace = trace_number(&want.trace_id);
        if let Some(problem) = trace_mismatch(
            want,
            got,
            state.unsettled(trace),
            copies_of(&copies.visible, trace),
        ) {
            panic!("{what}, trace {trace}: {problem}\nexpected {want:?}\nobserved {got:?}");
        }
    }
}

/// `rows` keyed by `key`, which must be unique.
fn keyed<T>(rows: impl IntoIterator<Item = (String, T)>, what: &str) -> BTreeMap<String, T> {
    let mut map = BTreeMap::new();
    for (key, row) in rows {
        assert!(
            map.insert(key.clone(), row).is_none(),
            "{what}: {key} is listed twice"
        );
    }
    map
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

/// `session_list` and `user_list` over all traces: the same keys, the trace counts, and the
/// session sums.
fn check_sessions_and_users(
    h: &Harness,
    state: &ProjectModel,
    copies: &Copies,
    project: P,
    at: &str,
) {
    let rows = state.expected_traces();
    let mut sessions: BTreeMap<String, Vec<&ExpectedTrace>> = BTreeMap::new();
    let mut users: BTreeMap<String, Vec<&ExpectedTrace>> = BTreeMap::new();
    for row in &rows {
        if let Some(session) = &row.session_id {
            sessions.entry(session.clone()).or_default().push(row);
        }
        if let Some(user) = &row.user_id {
            users.entry(user.clone()).or_default().push(row);
        }
    }

    let observed: BTreeMap<String, SessionRow> = keyed(
        observe_sessions(h, project)
            .into_iter()
            .map(|row| (row.session_id.clone(), row)),
        &format!("{at}: the session list"),
    );
    assert_eq!(
        observed.keys().collect::<Vec<_>>(),
        sessions.keys().collect::<Vec<_>>(),
        "{at}: the session ids differ from the model's"
    );
    for (session, members) in &sessions {
        let got = &observed[session];
        let want = Sums::of(members, copies);
        assert_eq!(
            got.trace_count, want.traces,
            "{at}: session {session} trace_count"
        );
        if let Some(problem) =
            sums_mismatch(&want, got.error_count, got.total_tokens, got.cost_total)
        {
            panic!("{at}: session {session}: {problem}\nobserved {got:?}");
        }
    }

    let observed_users: BTreeMap<String, i64> =
        keyed(observe_users(h, project), &format!("{at}: the user list"));
    assert_eq!(
        observed_users.keys().collect::<Vec<_>>(),
        users.keys().collect::<Vec<_>>(),
        "{at}: the user ids differ from the model's"
    );
    for (user, members) in &users {
        let count = i64::try_from(members.len()).expect("a model has few traces");
        assert_eq!(observed_users[user], count, "{at}: user {user} trace_count");
    }
}

/// One KPI, which must be listed once.
fn kpi(kpis: &[Kpi], name: KpiName) -> Kpi {
    let mut found = kpis.iter().filter(|kpi| kpi.name == name);
    let kpi = found
        .next()
        .unwrap_or_else(|| panic!("the metrics have no {name:?} KPI"));
    assert!(found.next().is_none(), "the metrics list {name:?} twice");
    *kpi
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

/// The KPIs over `window` and their `previous` values, which cover `previous_window(window)`.
/// The percentile is not compared (approximate); it must only be listed.
fn check_kpis(h: &Harness, state: &ProjectModel, project: P, window: (i64, i64), label: &str) {
    compare_kpis(state, window, &observe_kpis(h, project, window), label);
}

/// The KPIs `kpis` read over `window` against the model, as `check_kpis` compares them.
/// `FaultyRead` shares it (R4.5).
pub(super) fn compare_kpis(state: &ProjectModel, window: (i64, i64), kpis: &[Kpi], label: &str) {
    let copies = Copies::of(state);
    let current = expected_kpis(state, &copies, window);
    let previous = expected_kpis(state, &copies, previous_window(window));
    for ((name, want, exact), (_, want_previous, previous_exact)) in current.iter().zip(&previous) {
        let got = kpi(kpis, *name);
        assert!(
            float_sum_matches(*want, got.value, *exact),
            "{label}: the {name:?} KPI is {:?}, expected {want:?}",
            got.value
        );
        assert!(
            float_sum_matches(*want_previous, got.previous, *previous_exact),
            "{label}: the {name:?} KPI's previous value is {:?}, expected {want_previous:?}",
            got.previous
        );
    }
    let _ = kpi(kpis, KpiName::LatencyP95);
}

/// `filter_trace_ids` over `window`: the traces of the window's trace list, evaluation excluded.
fn check_filter_ids(
    h: &Harness,
    state: &ProjectModel,
    project: P,
    window: (i64, i64),
    label: &str,
) {
    let mut want_ids: Vec<String> = state
        .traces_in(Some(window), false)
        .into_iter()
        .map(|row| row.trace_id)
        .collect();
    want_ids.sort();
    assert_eq!(
        observe_filter_trace_ids(h, project, window),
        want_ids,
        "{label}: filter_trace_ids differs from the trace list"
    );
}

/// `alert_value` over `window`: the trace measures, and the average of each score name.
fn check_alerts(
    h: &Harness,
    state: &ProjectModel,
    copies: &Copies,
    project: P,
    window: (i64, i64),
    at: &str,
) {
    let members = state.traces_in(Some(window), false);
    let refs: Vec<&ExpectedTrace> = members.iter().collect();
    let sums = Sums::of(&refs, copies);
    let errors = i64::try_from(members.iter().filter(|row| row.error_count > 0).count())
        .expect("a model has few traces");

    let rate = observe_alert(h, project, window, AlertMeasure::ErrorRate);
    let want_rate = (sums.traces > 0).then(|| errors as f64 / sums.traces as f64);
    assert_eq!(rate.count, sums.traces, "{at}: the ErrorRate alert count");
    assert!(
        float_sum_matches(want_rate, rate.value, true),
        "{at}: the ErrorRate alert is {:?}, expected {want_rate:?}",
        rate.value
    );

    let cost = observe_alert(h, project, window, AlertMeasure::Cost);
    assert_eq!(cost.count, sums.traces, "{at}: the Cost alert count");
    assert!(
        float_sum_matches(
            Some(sums.cost_total.unwrap_or(0.0)),
            cost.value,
            sums.settled
        ),
        "{at}: the Cost alert is {:?}, expected {:?}",
        cost.value,
        sums.cost_total
    );

    for k in 0..3 {
        let name = format!("n{k}");
        let (count, mean) = expected_score_avg(state, window, &name);
        let got = observe_alert(
            h,
            project,
            window,
            AlertMeasure::ScoreAvg { name: name.clone() },
        );
        assert_eq!(got.count, count, "{at}: the ScoreAvg {name} alert count");
        // Exact even when a trace is unsettled. Duplicates add copies, not traces, so the
        // semi-join's trace set (and so the score set and its mean) is the same. That relies on
        // the shapes keeping `start_time`, `end_time` and `environment` identical across the
        // copies of a span: otherwise a copy could move a trace in or out of the window.
        assert!(
            float_sum_matches(mean, got.value, true),
            "{at}: the ScoreAvg {name} alert is {:?}, expected {mean:?}",
            got.value
        );
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

/// After `Settle`, the files on disk hold what the model predicts and nothing more (plan § R1.7):
/// no row a deletion covers, no row before the retention watermark, no file of a dropped project,
/// one partial per trace (two for class 2), and no staged object left.
pub(super) fn check_settled_files(h: &Harness, model: &Model) {
    let traces_by_id: BTreeMap<[u8; 16], u8> = (0..TRACES)
        .map(|trace| (*trace_id(trace).as_bytes(), trace))
        .collect();
    let keys_by_id: BTreeMap<String, ScoreKey> = score_key_space(model.projects())
        .into_iter()
        .map(|key| (score_id(&key).to_string(), key))
        .collect();
    for project in model.projects() {
        let state = model.project(project);
        check_settled_spans(h, state, project, &traces_by_id);
        check_settled_scores(h, state, project, &keys_by_id);
        check_settled_traces(h, state, project, &traces_by_id);
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
        let batch = parquet_rows(h.object_bytes(&file));
        let trace_bytes = trace_ids_of(&batch);
        let span_bytes = batch
            .column_by_name(spans::SPAN_ID)
            .expect("a spans file has a span_id column")
            .as_fixed_size_binary();
        let ingests = batch
            .column_by_name(spans::INGEST_TIME)
            .expect("a spans file has an ingest_time column")
            .as_primitive::<arrow::datatypes::TimestampNanosecondType>();
        let mut keys = BTreeSet::new();
        for (row, bytes) in trace_bytes.iter().enumerate() {
            let trace = *traces_by_id
                .get(bytes)
                .expect("a stored span belongs to a trace of the model");
            let ingest = ingests.value(row);
            assert!(
                !state.covers_span(trace, ingest),
                "project {project:?}: spans file {} keeps trace {trace} written at {ingest}, \
                 which a deletion covers",
                file.path
            );
            assert!(
                keys.insert((trace, span_bytes.value(row).to_vec())),
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
        let batch = parquet_rows(h.object_bytes(&file));
        let ids = batch
            .column_by_name(scores::SCORE_ID)
            .expect("a scores file has a score_id column")
            .as_string::<i32>();
        let deleted = batch
            .column_by_name(scores::DELETED)
            .expect("a scores file has a deleted column")
            .as_boolean();
        let ingests = batch
            .column_by_name(scores::INGEST_TIME)
            .expect("a scores file has an ingest_time column")
            .as_primitive::<arrow::datatypes::TimestampNanosecondType>();
        for row in 0..batch.num_rows() {
            let key = keys_by_id
                .get(ids.value(row))
                .expect("a stored score belongs to a key of the model");
            assert_eq!(
                key.project, project,
                "scores file {} of project {project:?} holds a score of {:?}",
                file.path, key.project
            );
            assert!(
                !deleted.value(row),
                "scores file {} keeps a deleted version after settle",
                file.path
            );
            assert!(
                !state.covers_score(key, ingests.value(row)),
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
            .query(project.id(), Default::default())
            .await
            .expect("query for the traces file check");
        let ctx =
            SessionContext::new_with_config_rt(SessionConfig::new(), Arc::clone(&query.runtime));
        ctx.register_table("traces_t", Arc::clone(&query.traces))
            .expect("register traces_t");
        let batches = ctx
            .sql("SELECT trace_id, partial_count FROM traces_t")
            .await
            .expect("select from traces_t")
            .collect()
            .await
            .expect("collect traces_t");
        let mut rows = Vec::new();
        for batch in batches {
            let ids = batch.column(0).as_fixed_size_binary();
            let partials = batch
                .column(1)
                .as_primitive::<arrow::datatypes::Int32Type>();
            for row in 0..batch.num_rows() {
                rows.push((hex(ids.value(row)), partials.value(row)));
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
