//! Error types for `cetra-column`.

use thiserror::Error;

/// Which budget refused a memory reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLimitKind {
    /// A spilling operator exceeded its fair share of the query cap.
    SpillShare,
    /// A query exceeded its per-query cap.
    QueryCap,
    /// The process-wide shared pool has no free bytes left.
    GlobalPool,
}

/// Errors produced by `cetra-column`. Never carries payload values.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ColumnError {
    /// Memory reservation exceeded its limit.
    #[error("memory limit ({kind:?}) exceeded: requested {requested} bytes, limit {limit} bytes")]
    MemoryLimit {
        /// Which budget refused the allocation.
        kind: MemoryLimitKind,
        /// Bytes requested.
        requested: usize,
        /// Budget limit in bytes.
        limit: usize,
    },
    /// Schema, column length, type, or invariant mismatch.
    #[error("invalid column or batch: {reason}")]
    InvalidBatch {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },
    /// Invalid or corrupt binary batch frame.
    #[error("invalid batch frame: {reason}")]
    InvalidFrame {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },
    /// Value out of range for a target type or offset width.
    #[error("value overflow: {reason}")]
    Overflow {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Invalid UTF-8 bytes or non-char-boundary offset in a string buffer.
    #[error("invalid utf-8 in column data")]
    InvalidUtf8,
}
