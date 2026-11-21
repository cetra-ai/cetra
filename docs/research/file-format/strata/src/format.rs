//! Constants and byte-level helpers shared by every layer of the format: a
//! bounds-checked little-endian reader ([`Cursor`]), a little-endian writer
//! ([`Out`]) and the 32-bit checksum.

use xxhash_rust::xxh3::Xxh3;

use crate::error::{Result, StrataError};

/// File magic, at offset 0 of the header and at the end of the postscript.
pub const FILE_MAGIC: &[u8; 4] = b"STRA";
/// Format version, written to the header and checked against the postscript.
pub const VERSION: u16 = 0;
/// Length of the file header: magic, version and flags.
pub const HEADER_LEN: u64 = 8;
/// Length of the postscript at the end of the file.
pub const POSTSCRIPT_LEN: u64 = 64;
/// Magic at the start of every segment.
pub const SEGMENT_MAGIC: u16 = 0x5347;
/// Length of a segment header.
pub const SEGMENT_HEADER_LEN: usize = 16;
/// Length of one column directory record.
pub const DIRECTORY_RECORD_LEN: usize = 64;
/// Length of one block-table entry.
pub const BLOCK_ENTRY_LEN: usize = 56;
/// Length of one block-table page reference.
pub const PAGE_REF_LEN: usize = 16;
/// Number of sections in the hot tail.
pub const HOT_SECTIONS: usize = 7;
/// Largest raw (decompressed) segment body the reader accepts and the writer writes.
pub const MAX_SEGMENT_RAW: u32 = 256 * 1024 * 1024;
/// Largest number of rows in one narrow chunk.
pub const MAX_CHUNK_ROWS: u32 = 65_535;
/// Largest number of list or map elements in one narrow chunk.
pub const MAX_CHUNK_ELEMENTS: usize = 1 << 20;
/// Largest number of value bytes a decoded narrow chunk may materialise.
pub const MAX_CHUNK_OUTPUT: usize = 256 * 1024 * 1024;
/// Largest number of block-table entries in one page.
pub const MAX_PAGE_ENTRIES: usize = 4_096;
/// Largest number of values in one dictionary.
pub const MAX_DICT_VALUES: usize = 1 << 20;
/// Largest number of keys of one `Map` column that are promoted to child columns.
pub const MAX_PROMOTED_KEYS: usize = 64;
/// Longest map key that may be promoted to a child column, in bytes.
pub const MAX_PROMOTED_KEY_LEN: usize = 1024;
/// Largest number of rows in one file: row ids are `u32`, and `row_count` itself must fit.
pub const MAX_ROWS: u64 = u32::MAX as u64;

/// The little-endian value of `bytes`, zero-extended to 64 bits. `None` when there are
/// more than eight bytes.
pub fn le_u64(bytes: &[u8]) -> Option<u64> {
    let mut buf = [0u8; 8];
    buf.get_mut(..bytes.len())?.copy_from_slice(bytes);
    Some(u64::from_le_bytes(buf))
}

/// The low 32 bits of the XXH3 64-bit hash of the concatenation of `parts`.
pub fn checksum32(parts: &[&[u8]]) -> u32 {
    let mut hasher = Xxh3::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.digest() as u32
}

/// Reads a whole Arrow IPC stream: its schema and every batch. `None` when the stream is
/// invalid, and also when arrow's reader panics: it panics on some invalid schemas (an unknown
/// type id, a bad bit width), so the call runs under `catch_unwind` and a corrupt stream
/// becomes an error instead of a crash.
pub fn read_ipc_stream(
    bytes: &[u8],
) -> Option<(arrow::datatypes::SchemaRef, Vec<arrow::array::RecordBatch>)> {
    if !ipc_messages_fit(bytes) {
        return None;
    }
    let read = || {
        let reader = arrow::ipc::reader::StreamReader::try_new(bytes, None).ok()?;
        let schema = reader.schema();
        let batches: Vec<_> = reader.collect::<std::result::Result<_, _>>().ok()?;
        Some((schema, batches))
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
        .ok()
        .flatten()
}

/// Whether every message of an Arrow IPC stream fits in `bytes`: its metadata length, and the
/// body length its metadata declares. arrow's stream reader allocates and zeroes both lengths
/// before reading them, so a stream of a few bytes that declares a gigabyte would cost a
/// gigabyte. The walk follows the reader's framing: an optional `0xFFFFFFFF` continuation
/// marker, an `i32` metadata length (0 ends the stream, as does the end of the bytes), the
/// metadata, then the body.
fn ipc_messages_fit(mut bytes: &[u8]) -> bool {
    let word = |bytes: &mut &[u8]| -> Option<[u8; 4]> {
        let (head, rest) = bytes.split_first_chunk::<4>()?;
        *bytes = rest;
        Some(*head)
    };
    loop {
        let Some(mut len) = word(&mut bytes) else {
            return true;
        };
        if len == [0xFF; 4] {
            let Some(next) = word(&mut bytes) else {
                return false;
            };
            len = next;
        }
        let Ok(len) = usize::try_from(i32::from_le_bytes(len)) else {
            return false;
        };
        if len == 0 {
            return true;
        }
        let Some((meta, rest)) = bytes.split_at_checked(len) else {
            return false;
        };
        let Ok(message) = arrow::ipc::root_as_message(meta) else {
            return false;
        };
        let Some(rest) = usize::try_from(message.bodyLength())
            .ok()
            .and_then(|body| rest.get(body..))
        else {
            return false;
        };
        bytes = rest;
    }
}

/// Whether a stored checksum matches the computed one. Under `cfg(fuzzing)` every checksum
/// matches, so a fuzzer's mutations reach the decoders behind it.
pub fn checksum_matches(computed: u32, stored: u32) -> bool {
    cfg!(fuzzing) || computed == stored
}

/// A bounds-checked little-endian reader over a byte slice.
///
/// `base` is the absolute file offset of `buf[0]`, used in error offsets. Every
/// method returns `Corrupt` with the caller's `what` when the bytes are missing.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
    base: u64,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `buf`, which starts at absolute offset `base`.
    pub fn new(buf: &'a [u8], base: u64) -> Self {
        Self { buf, pos: 0, base }
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Absolute file offset of the next byte.
    pub fn position(&self) -> u64 {
        self.base.saturating_add(self.pos as u64)
    }

    fn corrupt(&self, what: &'static str) -> StrataError {
        StrataError::Corrupt {
            what,
            offset: self.position(),
        }
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        let buf = self.buf;
        let end = self.pos.checked_add(n).ok_or_else(|| self.corrupt(what))?;
        let slice = buf.get(self.pos..end).ok_or_else(|| self.corrupt(what))?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N]> {
        let slice = self.bytes(N, what)?;
        slice.try_into().map_err(|_| self.corrupt(what))
    }

    /// The next byte.
    pub fn u8(&mut self, what: &'static str) -> Result<u8> {
        let [b] = self.array::<1>(what)?;
        Ok(b)
    }

    /// The next `u16`, little-endian.
    pub fn u16(&mut self, what: &'static str) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array::<2>(what)?))
    }

    /// The next `u32`, little-endian.
    pub fn u32(&mut self, what: &'static str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array::<4>(what)?))
    }

    /// The next `u64`, little-endian.
    pub fn u64(&mut self, what: &'static str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array::<8>(what)?))
    }
}

/// A little-endian byte writer. The writer builds the whole file in one `Out`
/// starting at byte 0, so `len()` is the absolute file offset.
pub struct Out(pub Vec<u8>);

impl Out {
    /// Appends one byte.
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    /// Appends a `u16`, little-endian.
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends a `u32`, little-endian.
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends a `u64`, little-endian.
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;

    use super::*;

    fn stream() -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![Some(1), None, Some(3)]))],
        )
        .unwrap();
        let mut out = Vec::new();
        let mut w = StreamWriter::try_new(&mut out, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        drop(w);
        out
    }

    #[test]
    fn an_ipc_stream_reads_its_schema_and_batches() {
        let (schema, batches) = read_ipc_stream(&stream()).unwrap();
        assert_eq!(schema.fields().len(), 1);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
    }

    /// Fuzz finding `fuzz/regressions/narrow_chunk/ipc-declared-length`: a stream of a few
    /// bytes that declares a large metadata length made arrow allocate it (1.6 GB, 27 s).
    #[test]
    fn an_ipc_stream_that_declares_more_than_it_holds_is_rejected() {
        let mut huge = vec![0xFF; 4];
        huge.extend_from_slice(&0x7FFF_FFF0_i32.to_le_bytes());
        huge.extend_from_slice(b"abcde");
        assert!(read_ipc_stream(&huge).is_none());
        // A body cut short: the last message declares more body than is left.
        let mut cut = stream();
        cut.truncate(cut.len() - 16);
        assert!(!ipc_messages_fit(&cut));
        assert!(read_ipc_stream(&cut).is_none());
        // A negative metadata length.
        assert!(read_ipc_stream(&(-5_i32).to_le_bytes()).is_none());
    }

    #[test]
    fn an_empty_or_ended_ipc_stream_fits() {
        assert!(ipc_messages_fit(&[]));
        assert!(ipc_messages_fit(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]));
        assert!(ipc_messages_fit(&stream()));
    }
}
