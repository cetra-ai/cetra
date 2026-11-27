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
    /// Caller request parameter outside the query text is invalid (`400 bad_request`).
    #[error("{0}")]
    InvalidRequest(String),
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
    /// Admission wait timed out or capacity saturated (`503 busy`).
    #[error("server busy")]
    Busy,
    /// Unexpected internal, I/O, or worker panic error (`500`).
    #[error("internal query error: {0}")]
    Internal(String),
    /// Corrupt or truncated data encountered during query execution (`500`).
    #[error("corrupt data: {0}")]
    CorruptData(String),
    /// Storage I/O or object-store failure encountered during query execution (`500`).
    #[error("storage read error: {0}")]
    StorageIo(String),
    /// Column invariant or internal error (never `ColumnError::MemoryLimit`, which converts to `TooLarge`).
    #[error("column error: {0}")]
    Column(ColumnError),
}

impl From<ColumnError> for QueryError {
    fn from(err: ColumnError) -> Self {
        match err {
            ColumnError::MemoryLimit { .. } => Self::TooLarge,
            ColumnError::Overflow { reason } => Self::Failed(reason.to_owned()),
            other => Self::Column(other),
        }
    }
}

impl QueryError {
    /// Builds `QueryError::InvalidRequest`, capping `message` at [`MESSAGE_CAP`].
    pub fn invalid_request(message: impl AsRef<str>) -> Self {
        Self::InvalidRequest(cap_message(message.as_ref()))
    }

    /// Builds `QueryError::Failed`, capping `message` at [`MESSAGE_CAP`].
    pub fn failed(message: impl AsRef<str>) -> Self {
        Self::Failed(cap_message(message.as_ref()))
    }

    /// Builds `QueryError::Internal`, capping `message` at [`MESSAGE_CAP`].
    pub fn internal(message: impl AsRef<str>) -> Self {
        Self::Internal(cap_message(message.as_ref()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_request_caps_utf8_message() {
        let short = QueryError::invalid_request("missing parameter");
        assert!(matches!(short, QueryError::InvalidRequest(ref m) if m == "missing parameter"));

        // Multi-byte UTF-8 character straddling MESSAGE_CAP boundary.
        let prefix = "a".repeat(MESSAGE_CAP - 1);
        let long = format!("{prefix}é");
        let capped = QueryError::invalid_request(&long);
        let QueryError::InvalidRequest(msg) = capped else {
            panic!("expected InvalidRequest");
        };
        assert_eq!(msg.len(), MESSAGE_CAP - 1);
        assert_eq!(msg, prefix);
    }

    #[test]
    fn column_overflow_maps_to_failed_and_typed_storage_errors_display() {
        let err = QueryError::from(ColumnError::Overflow {
            reason: "list offset overflow",
        });
        assert!(matches!(err, QueryError::Failed(ref m) if m == "list offset overflow"));

        let mem = QueryError::from(ColumnError::MemoryLimit {
            requested: 100,
            limit: 10,
            kind: cetra_column::MemoryLimitKind::QueryCap,
        });
        assert!(matches!(mem, QueryError::TooLarge));

        let corrupt = QueryError::CorruptData("bad footer".to_owned());
        assert_eq!(corrupt.to_string(), "corrupt data: bad footer");

        let io_err = QueryError::StorageIo("timeout".to_owned());
        assert_eq!(io_err.to_string(), "storage read error: timeout");
    }
}
