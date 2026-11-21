//! The error type shared by the writer and the reader.

use std::io;

/// Every error the Strata writer and reader can return.
///
/// Messages carry offsets, lengths, column ids and kinds, never row content.
#[derive(Debug, thiserror::Error)]
pub enum StrataError {
    /// The file or one of its parts is malformed; `what` names the failed check.
    #[error("corrupt file: {what} at offset {offset}")]
    Corrupt {
        /// Name of the failed check.
        what: &'static str,
        /// Absolute file offset where the check failed.
        offset: u64,
    },
    /// A type or layout that M1 cannot store; the message names the type, never values.
    #[error("unsupported: {what}")]
    Unsupported {
        /// What is unsupported.
        what: String,
    },
    /// Rows arrived out of order for the sort key.
    #[error("rows are not sorted by the sort key at row {row}")]
    Unsorted {
        /// Index of the first out-of-order row.
        row: u64,
    },
    /// The batch does not match the expected schema.
    #[error("schema mismatch: {reason}")]
    Schema {
        /// Why the schema does not match.
        reason: String,
    },
    /// More rows than a file can hold.
    #[error("too many rows: {rows} (limit 4294967295)")]
    TooManyRows {
        /// The rejected row count.
        rows: u64,
    },
    /// An Arrow error.
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    /// An object store error.
    #[error(transparent)]
    ObjectStore(#[from] object_store::Error),
    /// An I/O error.
    #[error("io: {action}")]
    Io {
        /// What was being done when the error occurred.
        action: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, StrataError>;
