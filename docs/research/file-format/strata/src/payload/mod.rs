//! The payload layer (S4). A long Utf8 or Binary column is cut into chunks, the chunks are
//! deduplicated and packed into zstd frames, each row keeps its list of chunk ids in refs
//! pages, and one payload header ties the parts together.

mod chunk;
mod frames;
mod header;
mod read;
mod refs;
mod search;
mod trigram;
mod varint;
mod write;

pub(crate) use read::read_payload_column;
pub use write::PayloadStats;
pub(crate) use write::{is_payload, write_payload_column};

/// Format constants of version 0.
///
/// Close a frame at this raw data size …
pub(crate) const FRAME_MAX_RAW: usize = 64 * 1024;
/// … or at this estimated compressed size.
pub(crate) const FRAME_MAX_EST: f64 = 16.0 * 1024.0;
/// Compression ratio assumed for the first frame.
pub(crate) const FRAME_INITIAL_RATIO: f64 = 4.0;
/// zstd level of payload frames.
pub(crate) const FRAME_ZSTD_LEVEL: i32 = 3;
/// Rows per refs page.
pub(crate) const REFS_PAGE_ROWS: usize = 4096;
/// A based refs record looks at most this many rows back.
pub(crate) const REFS_MAX_BACK: usize = 32;
/// Keyframe depth is 0; a based record has depth base + 1, at most this value.
pub(crate) const REFS_MAX_DEPTH: u8 = 15;
/// Length of the fixed part of the payload header.
pub(crate) const HEADER_FIXED_LEN: usize = 32;
/// Bytes of a chunk tail kept in the edge groups.
pub(crate) const EDGE: usize = 63;
/// Length of one edge group entry: `u8 tail_len` and `tail[63]`.
pub(crate) const EDGE_ENTRY_LEN: usize = 64;
/// Chunk ids per edge group segment.
pub(crate) const EDGE_GROUP_CHUNKS: usize = 1024;
/// Smallest frame filter, in bits.
pub(crate) const FILTER_MIN_BITS: u32 = 1024;
/// A frame filter may be at most this share of its frame's stored size.
pub(crate) const FILTER_MAX_SHARE: f64 = 0.5;
/// Bits of the file-level rollup (32 KiB).
pub(crate) const ROLLUP_BITS: u32 = 1 << 18;
/// Above this share of set bits the rollup is dropped.
pub(crate) const ROLLUP_MAX_FILL: f64 = 0.5;
/// Longest needle `payload_contains` accepts.
pub(crate) const MAX_NEEDLE: usize = EDGE + 1;
