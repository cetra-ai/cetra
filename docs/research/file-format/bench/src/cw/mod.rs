//! `fmtbench cw`: Strata against cetra's Parquet files on cetra's workload, without DataFusion
//! (`plans/08-cetra-workload-design.md`, `plans/08-m1-likely-failures.md`).
//!
//! The modules sit behind temporary features while they are built in parallel (M1.1 to M1.3);
//! M1.4 removes the features.

#[cfg(feature = "cw_cases")]
pub mod cases;
#[cfg(feature = "cw_harness")]
pub mod harness;
#[cfg(feature = "cw_read")]
pub mod read;

/// `fmtbench cw …`: the cetra workload benchmark (plans/08-m1-likely-failures.md).
#[cfg(feature = "cw_cases")]
pub async fn cmd(args: &[String]) -> crate::R<()> {
    match args.first().map(String::as_str) {
        Some("gen") => cases::gen_cmd(&args[1..]),
        Some("import") => cases::import_cmd(&args[1..]),
        _ => Err(crate::USAGE.into()),
    }
}
