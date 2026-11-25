//! The storage integration tests that `cargo test` runs, in one binary so that a storage edit
//! relinks one test executable instead of one per file.
//!
//! The benchmarks and the S3 probes (`tests/ingest_bench.rs`, `tests/trace_bench.rs`,
//! `tests/s3_chaos.rs`, `tests/s3_probe.rs`) are separate `test = false` targets in
//! `Cargo.toml`: they build only when named with `--test`.

// The shipped binary's allocator (D56), so that the soak's and the resource tests' memory is
// the memory `cetra` would use, not glibc malloc's.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod bench_support;
#[allow(dead_code)] // each test target uses part of the shared helpers
#[path = "../common/mod.rs"]
mod common;
mod crash;
mod s3;
mod soak;
