//! The checks after each op: storage's reads against the model's expected rows
//! (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! A failure panics with the op index, the project, and the expected and observed values.
//! proptest prints the shrunk op list.

use std::collections::{BTreeMap, BTreeSet};

use super::harness::Harness;
use super::model::{ExpectedScore, ExpectedTrace, Model, ProjectModel};
use super::observe::{
    ALL_ROWS, LatestScore, observe_latest, observe_scores, observe_scores_by_id,
    observe_span_detail, observe_spans, observe_trace_detail, observe_traces,
};
use super::ops::{SPANS, TRACES, score_key_space};
use super::shape::{P, hex, score_id, span_id, trace_id};
use crate::deletion::DeletionStatus;
use crate::reads::MAX_PAGE;

/// Visible copies per span key, as `ProjectModel::visible_spans` returns them.
type Visible = BTreeMap<(u8, u8), Vec<i64>>;

/// After an op that touched one project: that project's spans, trace list and scores.
pub(super) fn check_quick(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    check_span_keys(h, state, project, index);
    check_traces(h, state, project, index);
    check_scores(h, state, project, index);
}

/// After Restart, Crash and Settle, and at the end: both projects, the reads of
/// `check_quick`, plus the sampled details, the score reads by id and latest, and the
/// deletion statuses.
pub(super) fn check_full(h: &Harness, model: &mut Model, index: usize) {
    check_deletions(h, model, index);
    let model: &Model = model;
    for project in [P::A, P::B] {
        let state = model.project(project);
        check_quick(h, model, project, index);
        check_paging(h, project, index);
        check_samples(h, state, project, index);
        check_score_reads(h, model, project, index);
    }
}

/// Every registered deletion of both projects reads `Pending` or `Applied`. Once one reads
/// `Applied` it never reads `Pending` again (spec 03 §7). Dropped projects' deletions are
/// checked too: a drop marks them applied.
fn check_deletions(h: &Harness, model: &mut Model, index: usize) {
    for project in [P::A, P::B] {
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

/// `scores_by_id` over every score key either project can write (`score_key_space`) returns
/// exactly this project's live scores, so ids the model hides or never saw are tested too.
/// `latest_scores` returns the live score of each key.
fn check_score_reads(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    let ids: Vec<String> = score_key_space()
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
fn assert_same_scores(
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
    let mut observed: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (_, trace, span) in observe_spans(h, project, MAX_PAGE) {
        *observed.entry((trace, span)).or_default() += 1;
    }
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
    let visible = state.visible_spans();
    let mut expected = state.expected_traces();
    expected.sort_by(|a, b| (b.start_time, &b.trace_id).cmp(&(a.start_time, &a.trace_id)));

    let observed_ids: Vec<&str> = observed.iter().map(|row| row.trace_id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|row| row.trace_id.as_str()).collect();
    assert_eq!(
        observed_ids, expected_ids,
        "op {index}, project {project:?}: trace ids or their order differ"
    );
    for (want, got) in expected.iter().zip(&observed) {
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
