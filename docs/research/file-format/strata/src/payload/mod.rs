//! The payload layer (S4). A long Utf8 or Binary column is cut into chunks, the chunks are
//! deduplicated and packed into zstd frames, each row keeps its list of chunk ids in refs
//! pages, and one payload header ties the parts together.

mod chunk;
mod frames;
mod header;
mod read;
mod refs;
mod scope;
mod search;
mod trigram;
mod varint;
mod write;

#[cfg(test)]
pub(crate) use chunk::chunk_value;
pub(crate) use frames::decode_fetched_frame;
pub(crate) use header::PayloadHeader;
pub(crate) use read::{
    build_values, decode_frames, decode_header, frame_segments, frame_segments_of, ids_of,
    read_header, read_payload_column, read_rows, refs_segments,
};
pub use scope::{DedupScope, ScopeColumn, ScopeReport};
pub(crate) use search::payload_contains;
pub use write::PayloadStats;
pub(crate) use write::{
    CopiedFrame, PayloadCopy, SourceColumn, is_payload, write_payload_column, write_payload_merged,
};

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
/// Largest total number of chunk ids over the rows of one refs page, after tail sharing is
/// expanded. Bounds the memory a crafted page can make the reader allocate.
pub(crate) const MAX_PAGE_IDS: usize = 1 << 24;
/// Largest total number of chunk ids one read of a payload column may return. The writer
/// refuses a column with more chunk references, so every valid file reads back.
pub(crate) const MAX_READ_IDS: usize = 1 << 28;
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
