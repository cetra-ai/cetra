#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

pub mod lance_bench;
pub mod queries;
pub mod real_s3;
pub mod report;
pub mod s3_parquet;
pub mod schema;
pub mod strata_bench;
pub mod vortex_bench;
pub mod workloads;
