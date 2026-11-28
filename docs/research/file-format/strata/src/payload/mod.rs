//! The payload layer (S4). A long Utf8 or Binary column is cut into chunks, the chunks are
//! deduplicated and packed into zstd frames, each row keeps its list of chunk ids in refs
//! pages, and one payload header ties the parts together.

mod chunk;
mod copy;
mod gram;
mod header;
mod read;
mod refs;
mod search;
mod store;
mod varint;
mod write;

#[cfg(test)]
pub(crate) use chunk::chunk_value;
pub(crate) use copy::{MergeCopy, copy_stores, same_stores};
pub(crate) use header::PayloadHeader;
#[cfg(test)]
pub(crate) use read::frame_segments_of;
pub(crate) use read::{
    FrameBodies, build_payload_values, decode_header, ids_of, read_header, read_payload_column,
    refs_segments,
};
pub(crate) use search::{payload_contains, payload_contains_rows};
pub(crate) use store::{Dict, FrameEntry, PayloadStore};
pub use write::PayloadStats;
pub(crate) use write::{is_payload, write_payload_columns};

/// Format constants.
///
/// A frame is closed once it holds this many raw chunk bytes.
pub(crate) const FRAME_MAX_RAW: usize = 64 * 1024;
/// A store gets a zstd dictionary when its unique chunks hold at least this many bytes.
pub(crate) const DICT_MIN_RAW: usize = 4 << 20;
/// Size of a store's dictionary.
pub(crate) const DICT_MAX: usize = 64 << 10;
/// The dictionary is trained on the first this many bytes of evenly spread frames …
pub(crate) const DICT_SAMPLE: usize = 4096;
/// … up to about this many bytes in all.
pub(crate) const DICT_SAMPLE_BYTES: usize = 256 << 10;
/// Rows per refs page.
pub(crate) const REFS_PAGE_ROWS: usize = 1024;
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
/// Longest needle `payload_contains` accepts: one that starts in a chunk then ends within
/// `gram::WINDOW_HEAD` bytes of the next.
pub(crate) const MAX_NEEDLE: usize = gram::WINDOW_HEAD + 1;
