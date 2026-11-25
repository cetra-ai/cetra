//! Error types and message capping for `cetra-query`.

use cetra_column::ColumnError;
use thiserror::Error;

/// Maximum UTF-8 byte length of caller-facing query error messages (D94).
pub const MESSAGE_CAP: usize = 2048;

/// 1-based line and character column inside a filter or SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// 1-based line number.
    pub line: u32,
    /// 1-based character column.
    pub column: u32,
}

/// Caller-facing parse, validation, or binding error (HTTP 400 `invalid_filter`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct InvalidQuery {
    /// Capped message (`<= MESSAGE_CAP` bytes). Never logged with user data.
    pub message: String,
    /// Optional 1-based source location.
    pub position: Option<Position>,
}

impl InvalidQuery {
    /// Builds an [`InvalidQuery`], capping `message` at [`MESSAGE_CAP`] on a UTF-8 char boundary.
    pub fn new(message: impl AsRef<str>, position: Option<Position>) -> Self {
        Self {
            message: cap_message(message.as_ref()),
            position,
        }
    }
}

/// Errors returned by `cetra-query`.
#[derive(Debug, Error)]
pub enum QueryError {
    /// Parse, grammar, schema, or type error (400 `invalid_filter`).
    #[error("{0}")]
    Invalid(#[from] InvalidQuery),
    /// Runtime execution error: division by zero, overflow, bad cast, regex too big, non-finite cost (400 `query_failed`).
    #[error("{0}")]
    Failed(String),
    /// Memory pool cap exceeded (422 `query_too_large`).
    #[error("query memory limit exceeded")]
    TooLarge,
    /// Query was cancelled via `CancelToken` (maps to timeout/abort in server).
    #[error("query cancelled")]
    Cancelled,
    /// Column invariant or internal error (never `ColumnError::MemoryLimit`, which converts to `TooLarge`).
    #[error("column error: {0}")]
    Column(ColumnError),
}

impl From<ColumnError> for QueryError {
    fn from(err: ColumnError) -> Self {
        match err {
            ColumnError::MemoryLimit { .. } => Self::TooLarge,
            other => Self::Column(other),
        }
    }
}

impl QueryError {
    /// Builds `QueryError::Failed`, capping `message` at [`MESSAGE_CAP`].
    pub fn failed(message: impl AsRef<str>) -> Self {
        Self::Failed(cap_message(message.as_ref()))
    }
}

/// Truncates `text` to at most [`MESSAGE_CAP`] UTF-8 bytes on a char boundary.
pub fn cap_message(text: &str) -> String {
    if text.len() <= MESSAGE_CAP {
        return text.to_owned();
    }
    let mut end = MESSAGE_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
