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
