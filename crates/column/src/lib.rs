//! Pure-Rust columnar in-memory batches, encodings, memory pool, and binary framing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bitmap;
pub mod cancel;
pub mod error;
pub mod memory;
pub mod selection;
pub mod types;

pub use bitmap::Bitmap;
pub use cancel::CancelToken;
pub use error::{ColumnError, MemoryLimitKind};
pub use memory::{GlobalPool, Reservation, ScopedPool};
pub use selection::Selection;
pub use types::{
    COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, DataType, EventItem, Field, Interval, LinkItem,
    MAX_DECIMAL_PRECISION, MAX_NESTING_DEPTH, ScalarValue, Schema,
};
