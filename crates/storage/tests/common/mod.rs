//! Helpers shared by the storage benchmarks and the soak (track R M7,
//! docs/plans/r-hardening/07-m7-scale-soak.md).

pub(crate) mod corpus;
pub(crate) mod machine;
pub(crate) mod parquet_stats;
pub(crate) mod payload;
pub(crate) mod rng;
pub(crate) mod s3;
