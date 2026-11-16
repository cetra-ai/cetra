//! Track R M5: the storage under a full disk, a memory limit and few file
//! descriptors (docs/plans/r-hardening/05-m5-resources.md).
#![allow(dead_code)] // removed in R5.8, once every item is used

mod descriptors;
mod disk_full;
mod fill;
mod memory;
mod small_fs;

use super::check::{check_full, check_quick};
use super::check_both;
use super::harness::Harness;
use super::model::Model;
use super::ops::{Op, apply, settle};
use super::shape::P;

/// A/B ops every case applies before its pressure phase.
fn seed_ops() -> Vec<Op> {
    vec![
        Op::Append {
            project: P::A,
            trace: 0,
            count: 6,
            split: false,
        },
        Op::Append {
            project: P::B,
            trace: 0,
            count: 6,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 4,
            split: true,
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::Append {
            project: P::A,
            trace: 5,
            count: 6,
            split: false,
        },
        Op::Restart,
    ]
}

/// A/B ops every case applies after recovery.
fn after_ops() -> Vec<Op> {
    vec![
        Op::Append {
            project: P::A,
            trace: 2,
            count: 4,
            split: false,
        },
        Op::Resend {
            project: P::A,
            trace: 0,
            pick: 1,
        },
        Op::Score {
            project: P::B,
            target: 13,
            name: 1,
        },
        Op::Restart,
    ]
}

/// Applies `ops` with M1's `apply`, checking after each one as M1's `run_with` does: a restart, a
/// crash or a settle is checked in full, an op with a project quickly for that project, any other
/// op quickly for both projects. `first_index` numbers the ops in failure messages (100 for
/// `after_ops`, so they differ from `seed_ops`).
fn model_steps(
    h: &mut Harness,
    model: &mut Model,
    clock: &mut i64,
    ops: &[Op],
    first_index: usize,
) {
    for (number, op) in ops.iter().enumerate() {
        let index = first_index + number;
        apply(op, h, model, clock);
        match op {
            Op::Restart | Op::Crash | Op::Settle => check_full(h, model, index),
            _ => match op.project() {
                Some(project) => check_quick(h, model, project, index),
                None => check_both(h, model, index),
            },
        }
    }
}

/// M1's `settle`, then `check_full`: everything flushed, applied, compacted and checked physically.
fn settle_and_check(h: &mut Harness, model: &mut Model, clock: &mut i64) {
    settle(h, model, clock);
    check_full(h, model, 1_000);
}
