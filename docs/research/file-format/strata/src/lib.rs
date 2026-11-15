//! Strata: a file format for LLM traces, as a research prototype.
//!
//! Strata stores one table as one file, with narrow columns (chunked, encoded and
//! checksummed) and a hot tail that holds the schema, the column directory and the
//! block-table index. A reader opens a file from any `object_store` with one suffix
//! read.
//!
//! This crate is a research prototype outside the llmobs workspace. The design is in
//! `docs/research/file-format/prototype-plan.md` (one level up from this crate).
//!
//! The current limits are:
//! - sort-key lookup and key filters (M3.1); maps are shredded (M3.2);
//! - segment reads go in waves, coalesced by scan kind, and are counted (M4.1);
//! - there is no `scan`, no predicates and no pruning (M4.2, M4.3);
//! - there is no DataFusion integration (M5).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod format;
mod hot;
mod index;
mod io;
mod narrow;
mod payload;
mod segment;
mod shred;

mod config;
mod error;
mod file;
mod reader;
#[cfg(test)]
mod testgen;
mod writer;

pub use config::Config;
pub use error::{Result, StrataError};
pub use file::StrataFile;
pub use io::{IoStats, ScanKind};
pub use payload::PayloadStats;
pub use reader::read_column;
pub use writer::{ColumnSummary, FileSummary, StrataWriter};
