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
