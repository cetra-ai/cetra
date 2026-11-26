//! Errors returned by the query layer.
//!
//! [`RunError::Internal`]'s display text never includes the source error. Execution
//! failures carry their message on [`RunError::Failed`], which is returned to the
//! caller and never logged. Callers that log use [`RunError::kind`] (D94).

use crate::error::EngineError;

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
    /// The query engine failed on something that is not the caller's fault.
    ///
    /// Logged by kind only.
    #[error("internal query error")]
    Internal(#[source] EngineError),
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
