//! Helpers shared by the storage benchmarks and the soak (track R M7,
//! docs/plans/r-hardening/07-m7-scale-soak.md).

// The test targets that include these helpers allow the same lints at their roots; `tests/it`
// includes them under a module of its own, so they carry the allowances here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::print_stdout
)]

pub(crate) mod corpus;
pub(crate) mod machine;
pub(crate) mod parquet_stats;
pub(crate) mod payload;
pub(crate) mod rng;
pub(crate) mod s3;
