//! The checks after each op: storage's reads against the model's expected rows
//! (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! A failure panics with the op index, the project, and the expected and observed values.
//! proptest prints the shrunk op list.

use std::collections::{BTreeMap, BTreeSet};

use super::harness::Harness;
use super::model::{ExpectedTrace, Model, ProjectModel};
use super::observe::{
    ALL_ROWS, observe_span_detail, observe_spans, observe_trace_detail, observe_traces,
};
use super::ops::{SPANS, TRACES};
use super::shape::{P, hex, span_id, trace_id};
use crate::reads::MAX_PAGE;

/// Visible copies per span key, as `ProjectModel::visible_spans` returns them.
type Visible = BTreeMap<(u8, u8), Vec<i64>>;

/// After an op that touched one project: that project's spans and trace list.
pub(super) fn check_quick(h: &Harness, model: &Model, project: P, index: usize) {
    let state = model.project(project);
    check_span_keys(h, state, project, index);
    check_traces(h, state, project, index);
}

/// After Restart, Crash and Settle, and at the end: both projects, the reads of
/// `check_quick`, plus the sampled details.
pub(super) fn check_full(h: &Harness, model: &Model, index: usize) {
    for project in [P::A, P::B] {
        let state = model.project(project);
        check_quick(h, model, project, index);
        check_paging(h, project, index);
        check_samples(h, state, project, index);
    }
}

/// Reads in small pages return exactly the rows of one page, in the same order.
fn check_paging(h: &Harness, project: P, index: usize) {
    assert_eq!(
        observe_spans(h, project, 7),
        observe_spans(h, project, MAX_PAGE),
        "op {index}, project {project:?}: span rows read in pages of 7 differ from one page"
    );
    assert_eq!(
        observe_traces(h, project, ALL_ROWS, 2),
        observe_traces(h, project, ALL_ROWS, MAX_PAGE),
        "op {index}, project {project:?}: trace rows read in pages of 2 differ from one page"
    );
}

/// The `(trace, span)` keys of `span_list` equal the visible keys, and each key appears at
/// least once and at most once per visible copy.
fn check_span_keys(h: &Harness, state: &ProjectModel, project: P, index: usize) {
    let mut observed: BTreeMap<(String, String), usize> = BTreeMap::new();
    for key in observe_spans(h, project, MAX_PAGE) {
        *observed.entry(key).or_default() += 1;
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
    assert_eq!(
        observed.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "op {index}, project {project:?}: the span keys read differ from the model's visible keys"
    );
    for (key, count) in &observed {
        let copies = expected[key];
        assert!(
            *count >= 1 && *count <= copies,
            "op {index}, project {project:?}: span {key:?} is read {count} times, \
             the model has {copies} visible copies"
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
