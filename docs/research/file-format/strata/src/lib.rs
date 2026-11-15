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
//! The limits of M1 are:
//! - the writer stores every column through the narrow layer only, however long its
//!   values; the payload layer (M2) is not implemented;
//! - there are no key filters, map shredding or `lookup` (M3);
//! - there is no `scan`, no predicates, no pruning and no coalescing (M4);
//! - there is no DataFusion integration (M5).
//!
//! The hot tail sections 4, 5 and 6 are empty in M1.

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
mod narrow;
#[allow(dead_code)] // used by the writer from M2.2 on
mod payload;
mod segment;

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
pub use reader::read_column;
pub use writer::{ColumnSummary, FileSummary, StrataWriter};
