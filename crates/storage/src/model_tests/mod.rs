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
use ops::{Op, apply, settle};
use shape::{HOUR_NS, P, day, midnight};

/// Cases per run: `LLMOBS_MODEL_CASES`, else 16 (D-R-14).
fn cases() -> u32 {
    std::env::var("LLMOBS_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16)
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
            Op::Restart | Op::Crash | Op::Settle => check_full(&harness, &model, index),
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
    check_full(&harness, &model, ops.len());
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
