//! Strata: a file format for LLM traces, as a research prototype.
//!
//! Strata stores one table as one file, with narrow columns (chunked, encoded and
//! checksummed) and a hot tail that holds the schema, the column directory and the
//! block-table index. A reader opens a file from any `object_store` with one suffix
//! read.
//!
//! This crate is a research prototype outside the cetra workspace. The design is in
//! `docs/research/file-format/prototype-plan.md` (one level up from this crate).
//!
//! The current limits are:
//! - sort-key lookup and key filters (M3.1); maps are shredded (M3.2);
//! - segment reads go in waves, coalesced by scan kind, and are counted (M4.1);
//! - `scan` over narrow columns: predicates, zone and dictionary pruning, limits (M4.2);
//! - `scan` reads payload and map columns, with `Contains`, `MapKeyEq` and `lookup_scan`
//!   (M4.3);
//! - predicates are bounded to depth 64 and 4,096 nodes; a column read returns at most 2^28
//!   chunk ids; at most 64 ranged GETs of one wave are in flight; with a predicate, `limit`
//!   does not bound the bytes fetched (M4F);
//! - a DataFusion `TableProvider` with exact filter pushdown, `Inexact` `LIKE` as `Contains`,
//!   and `lookup_scan` for a `trace_id` equality (M5.1);
//! - `trace_detail` and `trace_list` fast paths, without DataFusion (M5.2);
//! - `merge` of sorted files (M6.1): every column is decoded, interleaved and re-encoded, and
//!   the row-source map costs 16 bytes per row; there is no input-count limit.

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
mod postings;
mod predicate;
mod scan;
mod segment;
mod shred;

mod config;
mod datafusion;
mod error;
mod fast;
mod file;
mod merge;
mod reader;
#[cfg(test)]
mod testgen;
mod writer;

pub use config::Config;
pub use datafusion::{StrataExec, StrataTable, table_schema};
pub use error::{Result, StrataError};
pub use fast::{trace_detail, trace_list};
pub use file::StrataFile;
pub use io::{IoStats, ScanKind};
pub use merge::merge;
pub use payload::{DedupScope, PayloadStats, ScopeColumn, ScopeReport};
pub use predicate::{CmpOp, Lit, Predicate};
pub use reader::read_column;
pub use scan::ScanRequest;
pub use writer::{ColumnSummary, FileSummary, StrataWriter};
