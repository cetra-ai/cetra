//! Errors returned by the query layer.
//!
//! [`RunError::Internal`]'s display text never includes the source error. Execution
//! failures carry their message on [`RunError::Failed`], which is returned to the
//! caller and never logged. Callers that log use [`RunError::kind`] (D94).

use std::error::Error as StdError;
use std::io;

use datafusion::arrow::error::ArrowError;
use datafusion::error::DataFusionError;

use crate::query_text::{cap_message, planning_failure};

/// 1-based line and column, counted in characters (spec 09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Line, starting at 1.
    pub line: u32,
    /// Column, starting at 1.
    pub column: u32,
}

/// Filter or SQL text the caller sent is invalid.
///
/// The server maps this to 400 `invalid_filter`. The message may quote the
/// caller's text; it is returned to the caller and never logged (D94).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidQuery {
    /// Caller-facing explanation, capped at 2,048 bytes.
    pub message: String,
    /// Where the problem starts, when one is known.
    pub position: Option<Position>,
}

/// A query could not be built or run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A request parameter outside the query text is invalid.
    ///
    /// The server maps this to 400 `bad_request`.
    #[error("{0}")]
    InvalidRequest(String),
    /// The caller's filter or SQL text is invalid.
    #[error(transparent)]
    Invalid(#[from] InvalidQuery),
    /// The query ran and failed. The message is for the caller (D94).
    ///
    /// The server maps this to 400 `query_failed`.
    #[error("{0}")]
    Failed(String),
    /// The query exceeded its time limit.
    ///
    /// The server maps this to 422 `query_timeout`.
    #[error("the query took longer than its time limit")]
    Timeout,
    /// The query needed more memory than its pool allows, or more spill space than the disk
    /// has above `storage.min_free_bytes`.
    ///
    /// The server maps this to 422 `query_too_large`.
    #[error("the query needed more memory or temporary disk space than allowed")]
    TooLarge,
    /// DataFusion failed on something that is not the caller's fault.
    ///
    /// Logged by kind only.
    #[error("internal query error")]
    Internal(#[source] DataFusionError),
}

/// Map a DataFusion error to the caller's [`RunError`].
///
/// `Context`, `Shared` and `Diagnostic` are wrappers. The loop unwraps all
/// three before matching the root. `find_root` walks `source()`, which does
/// cover `Diagnostic` in 55.2, but the match stays here so a future wrapper
/// change is visible.
pub(crate) fn classify(error: DataFusionError) -> RunError {
    let root = root(&error);
    match root {
        DataFusionError::ResourcesExhausted(_) => RunError::TooLarge,
        _ if spill_exhausted(root) => RunError::TooLarge,
        DataFusionError::Plan(_)
        | DataFusionError::SchemaError(_, _)
        | DataFusionError::SQL(_, _)
        | DataFusionError::NotImplemented(_) => RunError::Invalid(planning_failure(&error)),
        DataFusionError::Execution(_)
        | DataFusionError::ArrowError(_, _)
        | DataFusionError::Internal(_) => RunError::Failed(cap_message(&root.strip_backtrace())),
        _ => RunError::Internal(error),
    }
}

/// DataFusion's spill limit text (`datafusion-execution` 55.2, `FileSpillWriter::write`).
/// `a_spill_past_the_cap_fails_as_too_large` breaks if an upgrade changes it.
const SPILL_LIMIT_TEXT: &str = "exceeded the allowable limit";

/// How deep the walk goes through wrapped errors. The chains seen are three or four long.
const MAX_DEPTH: u8 = 8;

/// The query ran out of spill space: DataFusion's spill limit, `ENOSPC` or `EDQUOT`, found in
/// the I/O errors under `error`.
fn spill_exhausted(error: &DataFusionError) -> bool {
    exhausted_in_datafusion(error, 0)
}

fn exhausted_in_datafusion(error: &DataFusionError, depth: u8) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match error {
        DataFusionError::IoError(error) => io_exhausted(error, depth + 1),
        DataFusionError::ArrowError(arrow, _) => match arrow.as_ref() {
            ArrowError::IoError(_, error) => io_exhausted(error, depth + 1),
            ArrowError::ExternalError(source) => exhausted_in_source(source.as_ref(), depth + 1),
            _ => false,
        },
        DataFusionError::External(source) => exhausted_in_source(source.as_ref(), depth + 1),
        _ => false,
    }
}

/// A wrapped error that is a DataFusion error or an I/O error.
fn exhausted_in_source(source: &(dyn StdError + Send + Sync + 'static), depth: u8) -> bool {
    if let Some(error) = source.downcast_ref::<DataFusionError>() {
        return exhausted_in_datafusion(error, depth);
    }
    if let Some(error) = source.downcast_ref::<io::Error>() {
        return io_exhausted(error, depth);
    }
    false
}

/// `kind()` is `StorageFull` or `QuotaExceeded`, or the text has `SPILL_LIMIT_TEXT`, or
/// `get_ref()` holds a DataFusion error or an I/O error that does. `io::Error::source()`
/// skips the custom payload, so `get_ref()` is the link followed.
fn io_exhausted(error: &io::Error, depth: u8) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) || error.to_string().contains(SPILL_LIMIT_TEXT)
    {
        return true;
    }
    depth < MAX_DEPTH
        && error
            .get_ref()
            .is_some_and(|inner| exhausted_in_source(inner, depth + 1))
}

/// The error under every `Context`, `Shared` and `Diagnostic` wrapper.
pub(crate) fn root(error: &DataFusionError) -> &DataFusionError {
    let mut current = error;
    loop {
        current = match current {
            DataFusionError::Context(_, inner) | DataFusionError::Diagnostic(_, inner) => inner,
            DataFusionError::Shared(inner) => inner.as_ref(),
            _ => return current,
        };
    }
}

impl RunError {
    /// Stable kind for logs. Never the message.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::Invalid(_) => "invalid",
            Self::Failed(_) => "failed",
            Self::Timeout => "timeout",
            Self::TooLarge => "too_large",
            Self::Internal(_) => "internal",
        }
    }
}
