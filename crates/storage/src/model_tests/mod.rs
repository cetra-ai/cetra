#![allow(dead_code)] // removed in R1.7, once every item is used
//! Track R M1: random operations against `Storage` and an in-memory model of
//! every read (docs/plans/r-hardening/01-m1-model.md).

mod check;
mod harness;
mod model;
mod observe;
mod ops;
mod shape;

use proptest::prelude::*;

use check::{check_full, check_quick};
use harness::Harness;
use model::Model;
use observe::{ALL_ROWS, observe_spans, observe_traces};
use ops::{Op, apply, settle};
use shape::{HOUR_NS, P, day, midnight};

use crate::reads::MAX_PAGE;

/// Cases per run: `LLMOBS_MODEL_CASES`, else 8 (D-R-14).
fn cases() -> u32 {
    std::env::var("LLMOBS_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random ops on two projects read back exactly as the model predicts.
    #[test]
    fn storage_matches_the_model(ops in ops::ops()) {
        run_case(&ops);
    }
}

fn run_case(ops: &[Op]) {
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for (index, op) in ops.iter().enumerate() {
        apply(op, &mut harness, &mut model, &mut clock);
        match op {
            Op::Restart | Op::Crash | Op::Settle => check_full(&harness, &mut model, index),
            _ => match op.project() {
                Some(project) => check_quick(&harness, &model, project, index),
                None => {
                    check_quick(&harness, &model, P::A, index);
                    check_quick(&harness, &model, P::B, index);
                }
            },
        }
    }
    settle(&mut harness, &mut model, &mut clock);
    check_full(&harness, &mut model, ops.len());
}

/// Dropping B leaves A's reads as the model has them, and B's reads empty (spec 03 §7).
#[test]
fn drop_keeps_project_a() {
    let ops = [
        Op::Append {
            project: P::A,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::B,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 2,
            split: true,
        },
        Op::Append {
            project: P::B,
            trace: 1,
            count: 2,
            split: false,
        },
        Op::DropB,
        Op::Settle,
    ];
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in &ops {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    check_full(&harness, &mut model, ops.len());
    assert!(
        !model.a.expected_traces().is_empty(),
        "project A has traces to check"
    );
    assert!(
        observe_spans(&harness, P::B, MAX_PAGE).is_empty(),
        "dropped project B still lists spans"
    );
    assert!(
        observe_traces(&harness, P::B, ALL_ROWS, MAX_PAGE).is_empty(),
        "dropped project B still lists traces"
    );
}

/// Score ops leave live scores behind, and storage reads them back as the model has them.
/// Without this, the score checks could pass on runs where every score is deleted or hidden.
#[test]
fn score_ops_produce_live_scores() {
    let ops = [
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        // Older version, then the newest version again at a later ingest.
        Op::StaleScore {
            project: P::A,
            pick: 0,
            back: 1,
        },
        Op::StaleScore {
            project: P::A,
            pick: 0,
            back: 0,
        },
        Op::Score {
            project: P::A,
            target: 12,
            name: 1,
        },
        // Deletes the first key in key order (trace 0, name 0): version 3, deleted.
        Op::DeleteScore {
            project: P::A,
            pick: 0,
        },
        Op::Score {
            project: P::B,
            target: 1,
            name: 2,
        },
        Op::Restart,
        // Session 0, name 1 again, at version 1 (a tie with the stored version 1).
        Op::StaleScore {
            project: P::A,
            pick: 1,
            back: 2,
        },
        Op::Score {
            project: P::A,
            target: 12,
            name: 1,
        },
        Op::Score {
            project: P::A,
            target: 5,
            name: 2,
        },
        Op::StaleScore {
            project: P::B,
            pick: 0,
            back: 4,
        },
    ];
    run_case(&ops);

    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in &ops {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    let live = model.a.expected_scores().len() + model.b.expected_scores().len();
    assert!(live > 0, "the fixed score ops leave no live score");
    check_quick(&harness, &model, P::A, ops.len());
    check_quick(&harness, &model, P::B, ops.len());
}

/// The shrunk case of finding M1-1: a re-sent span of trace 7 is kept twice, and paging the span
/// list at 7 rows used to drop one copy at a page boundary (spec 01: list views de-duplicate).
#[test]
fn regression_m1_1_span_paging_with_duplicates() {
    run_case(&[
        Op::Append {
            project: P::A,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 4,
            count: 5,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 0,
            count: 5,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 4,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 3,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 7,
            count: 1,
            split: true,
        },
        Op::Resend {
            project: P::A,
            trace: 7,
            pick: 218,
        },
        Op::Append {
            project: P::A,
            trace: 0,
            count: 4,
            split: true,
        },
        Op::Crash,
    ]);
}
