//! `fmtbench cw`: Strata against cetra's Parquet files on cetra's workload, without DataFusion
//! (`plans/08-cetra-workload-design.md`, `plans/08-m1-likely-failures.md`).

pub mod cases;
pub mod harness;
pub mod many;
pub mod project;
pub mod read;
pub mod writes;

/// `fmtbench cw …`: the cetra workload benchmark (plans/08-m1-likely-failures.md).
pub async fn cmd(args: &[String]) -> crate::R<()> {
    match args.first().map(String::as_str) {
        Some("gen") => cases::gen_cmd(&args[1..]),
        Some("import") => cases::import_cmd(&args[1..]),
        Some("split") => cases::split_cmd(&args[1..]),
        Some("flush-input") => cases::flush_input_cmd(&args[1..]),
        Some("project") => project::project_cmd(&args[1..]).await,
        Some("layout") => project::layout_cmd(&args[1..]),
        Some("run") => harness::run_cmd(&args[1..]).await,
        Some("one") => harness::one_cmd(&args[1..]).await,
        _ => Err(crate::USAGE.into()),
    }
}
