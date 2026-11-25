//! Pure-Rust columnar in-memory batches, encodings, memory pool, and binary framing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bitmap;
pub mod error;
pub mod selection;
pub mod types;

pub use bitmap::Bitmap;
pub use error::{ColumnError, MemoryLimitKind};
pub use selection::Selection;
pub use types::{
    COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, DataType, EventItem, Field, Interval, LinkItem,
    MAX_DECIMAL_PRECISION, MAX_NESTING_DEPTH, ScalarValue, Schema,
};
