//! The file header, the postscript and the hot tail (S1, S2).
//!
//! The hot tail has seven sections, each a `u32` length followed by its bytes:
//! the Arrow schema, the column directory, the sort-key block table, the
//! sort-key filter, the shredded map keys, the key filter index and the
//! block-table page index.

use std::collections::HashSet;

use arrow::datatypes::SchemaRef;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;

use crate::error::{Result, StrataError};
use crate::format::{
    BLOCK_ENTRY_LEN, Cursor, DIRECTORY_RECORD_LEN, FILE_MAGIC, HOT_SECTIONS, Out, PAGE_REF_LEN,
    VERSION,
};
use crate::segment::SegmentRef;

/// Writes the 8-byte file header: magic, version and flags 0.
pub fn write_file_header(out: &mut Out) {
    out.bytes(FILE_MAGIC);
    out.u16(VERSION);
    out.u16(0);
}

/// The 64-byte postscript at the end of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Postscript {
    /// Absolute offset of the hot tail.
    pub hot_offset: u64,
    /// Length of the hot tail in bytes.
    pub hot_len: u32,
    /// [`checksum32`] of the hot tail.
    pub hot_checksum: u32,
    /// Number of rows in the file.
    pub row_count: u64,
    /// Number of column directory records.
    pub n_columns: u32,
    /// Format version, equal to [`VERSION`].
    pub version: u16,
    /// Format flags, equal to 0.
    pub flags: u16,
    /// Index of the first sort-key column, or `u32::MAX` for none.
    pub sort_key_column: u32,
}

impl Postscript {
    /// Writes exactly 64 bytes: the fields in order, 24 zero bytes and the file magic.
    pub fn encode(&self, out: &mut Out) {
        out.u64(self.hot_offset);
        out.u32(self.hot_len);
        out.u32(self.hot_checksum);
        out.u64(self.row_count);
        out.u32(self.n_columns);
        out.u16(self.version);
        out.u16(self.flags);
        out.u32(self.sort_key_column);
        out.bytes(&[0u8; 24]);
        out.bytes(FILE_MAGIC);
    }

    /// Decodes and checks a postscript read from `offset`.
    pub fn decode(bytes: &[u8; 64], offset: u64) -> Result<Self> {
        let mut c = Cursor::new(bytes.as_slice(), offset);
        let hot_offset = c.u64("postscript")?;
        let hot_len = c.u32("postscript")?;
        let hot_checksum = c.u32("postscript")?;
        let row_count = c.u64("postscript")?;
        let n_columns = c.u32("postscript")?;
        let version = c.u16("postscript")?;
        let flags = c.u16("postscript")?;
        let sort_key_column = c.u32("postscript")?;
        if c.bytes(24, "postscript")?.iter().any(|b| *b != 0) {
            return Err(StrataError::Corrupt {
                what: "postscript reserved",
                offset: c.position(),
            });
        }
        if c.bytes(4, "postscript")? != FILE_MAGIC.as_slice() {
            return Err(StrataError::Corrupt {
                what: "postscript magic",
                offset: c.position(),
            });
        }
        if version != VERSION {
            return Err(StrataError::Corrupt {
                what: "postscript version",
                offset,
            });
        }
        if flags != 0 {
            return Err(StrataError::Corrupt {
                what: "postscript flags",
                offset,
            });
        }
        Ok(Self {
            hot_offset,
            hot_len,
            hot_checksum,
            row_count,
            n_columns,
            version,
            flags,
            sort_key_column,
        })
    }
}

/// What a column's data is stored as (S2 `layer`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// A narrow column.
    Narrow = 0,
    /// A payload column.
    Payload = 1,
    /// A shredded map parent, with no data of its own.
    MapParent = 2,
    /// A shredded map child, with narrow rules.
    MapChild = 3,
    /// A residual map, with narrow rules and `IPC` encoding.
    Residual = 4,
}

impl Layer {
    /// The layer for a stored byte, or `None` if the value is unknown.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Narrow),
            1 => Some(Self::Payload),
            2 => Some(Self::MapParent),
            3 => Some(Self::MapChild),
            4 => Some(Self::Residual),
            _ => None,
        }
    }
}

/// One 64-byte column directory record (S2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryRecord {
    /// Schema index, or a higher id for hidden map children.
    pub column_id: u32,
    /// Parent column id, or `u32::MAX` for none.
    pub parent: u32,
    /// How the column is stored.
    pub layer: Layer,
    /// Bit 0: a sorted dictionary is present.
    pub flags: u8,
    /// Byte offset of this column's page list within hot-tail section 7.
    pub pages_offset: u32,
    /// Number of block-table pages.
    pub n_pages: u32,
    /// Number of chunks.
    pub n_chunks: u32,
    /// Offset of the dictionary segment, 0 for none.
    pub dict_offset: u64,
    /// Length of the dictionary segment.
    pub dict_len: u32,
    /// Offset of the payload header segment, 0 unless the layer is payload.
    pub payload_offset: u64,
    /// Length of the payload header segment.
    pub payload_len: u32,
    /// Number of null values in the column.
    pub null_count: u64,
}

impl DirectoryRecord {
    /// Writes exactly 64 bytes. The reserved fields are written as zero.
    pub fn encode(&self, out: &mut Out) {
        out.u32(self.column_id);
        out.u32(self.parent);
        out.u8(self.layer as u8);
        out.u8(self.flags);
        out.u16(0);
        out.u32(self.pages_offset);
        out.u32(self.n_pages);
        out.u32(self.n_chunks);
        out.u64(self.dict_offset);
        out.u32(self.dict_len);
        out.u64(self.payload_offset);
        out.u32(self.payload_len);
        out.u64(self.null_count);
        out.u64(0);
    }

    /// Decodes one record. Both reserved fields must be zero, and the layer must be known.
    pub fn decode(c: &mut Cursor<'_>) -> Result<Self> {
        let column_id = c.u32("directory record")?;
        let parent = c.u32("directory record")?;
        let layer_offset = c.position();
        let layer = Layer::from_u8(c.u8("directory record")?).ok_or(StrataError::Corrupt {
            what: "directory layer",
            offset: layer_offset,
        })?;
        let flags = c.u8("directory record")?;
        if c.u16("directory record")? != 0 {
            return Err(StrataError::Corrupt {
                what: "directory reserved",
                offset: c.position(),
            });
        }
        let pages_offset = c.u32("directory record")?;
        let n_pages = c.u32("directory record")?;
        let n_chunks = c.u32("directory record")?;
        let dict_offset = c.u64("directory record")?;
        let dict_len = c.u32("directory record")?;
        let payload_offset = c.u64("directory record")?;
        let payload_len = c.u32("directory record")?;
        let null_count = c.u64("directory record")?;
        if c.u64("directory record")? != 0 {
            return Err(StrataError::Corrupt {
                what: "directory reserved",
                offset: c.position(),
            });
        }
        Ok(Self {
            column_id,
            parent,
            layer,
            flags,
            pages_offset,
            n_pages,
            n_chunks,
            dict_offset,
            dict_len,
            payload_offset,
            payload_len,
            null_count,
        })
    }
}

/// One block-table page reference in hot-tail section 7 (16 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRef {
    /// First row covered by the page.
    pub first_row: u32,
    /// Absolute offset of the page segment.
    pub offset: u64,
    /// Length of the page segment.
    pub len: u32,
}

impl PageRef {
    /// Writes exactly 16 bytes.
    pub fn encode(&self, out: &mut Out) {
        out.u32(self.first_row);
        out.u64(self.offset);
        out.u32(self.len);
    }

    /// Decodes one reference.
    pub fn decode(c: &mut Cursor<'_>) -> Result<Self> {
        Ok(Self {
            first_row: c.u32("page ref")?,
            offset: c.u64("page ref")?,
            len: c.u32("page ref")?,
        })
    }
}

/// The hot tail: the seven sections, decoded.
pub struct HotTail {
    /// The table schema (section 1).
    pub schema: SchemaRef,
    /// The column directory (section 2).
    pub directory: Vec<DirectoryRecord>,
    /// Section 3: sort-key block table entries, raw (56-byte entries).
    pub sort_key_entries: Vec<u8>,
    /// Section 4: the body of the sort-key filter. Empty when the first sort-key column has no
    /// filter type.
    pub sort_key_filter: Vec<u8>,
    /// Section 5: shredded map keys, empty until map shredding (M3.2b).
    pub map_keys: Vec<u8>,
    /// Section 6: the key-filter segments of the other filtered columns, by column id.
    pub key_filters: Vec<(u32, SegmentRef)>,
    /// Section 7: block-table page references, raw (16-byte entries).
    pub pages: Vec<u8>,
}

/// Bytes of one section-6 entry: `u32 column_id · u64 offset · u32 len`.
const KEY_FILTER_ENTRY_LEN: usize = 16;

/// Decodes section 6. Its length is `4 + 16 × n`, every column id is below `n_fields`, and no
/// column id appears twice.
fn decode_key_filter_index(
    bytes: &[u8],
    base: u64,
    n_fields: usize,
) -> Result<Vec<(u32, SegmentRef)>> {
    let corrupt = || StrataError::Corrupt {
        what: "key filter index",
        offset: base,
    };
    let mut c = Cursor::new(bytes, base);
    let n = c.u32("key filter index")? as usize;
    let body = bytes.len().checked_sub(4).ok_or_else(corrupt)?;
    if n.checked_mul(KEY_FILTER_ENTRY_LEN) != Some(body) {
        return Err(corrupt());
    }
    let mut seen = HashSet::with_capacity(n);
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let column_id = c.u32("key filter index")?;
        let offset = c.u64("key filter index")?;
        let len = c.u32("key filter index")?;
        let in_schema = usize::try_from(column_id).is_ok_and(|id| id < n_fields);
        if !in_schema || !seen.insert(column_id) {
            return Err(corrupt());
        }
        entries.push((column_id, SegmentRef { offset, len }));
    }
    Ok(entries)
}

fn write_section(out: &mut Out, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).map_err(|_| StrataError::Unsupported {
        what: format!("hot tail section of {} bytes", body.len()),
    })?;
    out.u32(len);
    out.bytes(body);
    Ok(())
}

/// Reads one `u32`-prefixed section. Returns the absolute offset of its bytes and the bytes.
fn read_section<'a>(c: &mut Cursor<'a>) -> Result<(u64, &'a [u8])> {
    let len = c.u32("hot section length")? as usize;
    let base = c.position();
    Ok((base, c.bytes(len, "hot section")?))
}

impl HotTail {
    /// Writes the seven sections in order.
    pub fn encode(&self, out: &mut Out) -> Result<()> {
        let mut schema_stream = Vec::new();
        let mut writer = StreamWriter::try_new(&mut schema_stream, &self.schema)?;
        writer.finish()?;
        drop(writer);

        let mut directory = Out(Vec::with_capacity(
            self.directory.len() * DIRECTORY_RECORD_LEN,
        ));
        for record in &self.directory {
            record.encode(&mut directory);
        }

        let count =
            u32::try_from(self.key_filters.len()).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion key filters".to_string(),
            })?;
        let mut key_filter_index = Out(Vec::with_capacity(
            4 + self.key_filters.len() * KEY_FILTER_ENTRY_LEN,
        ));
        key_filter_index.u32(count);
        for (column_id, segment) in &self.key_filters {
            key_filter_index.u32(*column_id);
            key_filter_index.u64(segment.offset);
            key_filter_index.u32(segment.len);
        }

        let sections: [&[u8]; HOT_SECTIONS] = [
            schema_stream.as_slice(),
            directory.0.as_slice(),
            self.sort_key_entries.as_slice(),
            self.sort_key_filter.as_slice(),
            self.map_keys.as_slice(),
            key_filter_index.0.as_slice(),
            self.pages.as_slice(),
        ];
        for section in sections {
            write_section(out, section)?;
        }
        Ok(())
    }

    /// Decodes and checks the hot tail whose bytes start at file offset `offset`.
    pub fn decode(bytes: &[u8], offset: u64) -> Result<Self> {
        let mut c = Cursor::new(bytes, offset);

        let (schema_base, schema_bytes) = read_section(&mut c)?;
        let hot_schema = |_: arrow::error::ArrowError| StrataError::Corrupt {
            what: "hot schema",
            offset: schema_base,
        };
        let mut reader = StreamReader::try_new(schema_bytes, None).map_err(hot_schema)?;
        let schema = reader.schema();
        if reader.next().is_some() {
            return Err(StrataError::Corrupt {
                what: "hot schema",
                offset: schema_base,
            });
        }

        let (dir_base, dir_bytes) = read_section(&mut c)?;
        if !dir_bytes.len().is_multiple_of(DIRECTORY_RECORD_LEN) {
            return Err(StrataError::Corrupt {
                what: "hot directory length",
                offset: dir_base,
            });
        }
        let mut dir_cursor = Cursor::new(dir_bytes, dir_base);
        let mut directory = Vec::with_capacity(dir_bytes.len() / DIRECTORY_RECORD_LEN);
        while dir_cursor.remaining() > 0 {
            directory.push(DirectoryRecord::decode(&mut dir_cursor)?);
        }

        let (sort_base, sort_key_entries) = read_section(&mut c)?;
        if !sort_key_entries.len().is_multiple_of(BLOCK_ENTRY_LEN) {
            return Err(StrataError::Corrupt {
                what: "hot sort-key length",
                offset: sort_base,
            });
        }
        let (_, sort_key_filter) = read_section(&mut c)?;
        let (_, map_keys) = read_section(&mut c)?;
        let (index_base, key_filter_index) = read_section(&mut c)?;
        let key_filters =
            decode_key_filter_index(key_filter_index, index_base, schema.fields().len())?;

        let (pages_base, pages) = read_section(&mut c)?;
        if !pages.len().is_multiple_of(PAGE_REF_LEN) {
            return Err(StrataError::Corrupt {
                what: "hot page length",
                offset: pages_base,
            });
        }
        if c.remaining() != 0 {
            return Err(StrataError::Corrupt {
                what: "hot trailing bytes",
                offset: c.position(),
            });
        }

        Ok(Self {
            schema,
            directory,
            sort_key_entries: sort_key_entries.to_vec(),
            sort_key_filter: sort_key_filter.to_vec(),
            map_keys: map_keys.to_vec(),
            key_filters,
            pages: pages.to_vec(),
        })
    }

    /// The page references of one column, decoded from section 7.
    pub fn pages_of(&self, rec: &DirectoryRecord) -> Result<Vec<PageRef>> {
        let corrupt = || StrataError::Corrupt {
            what: "page list",
            offset: u64::from(rec.pages_offset),
        };
        let start = rec.pages_offset as usize;
        let n = rec.n_pages as usize;
        if !start.is_multiple_of(PAGE_REF_LEN) {
            return Err(corrupt());
        }
        let len = n.checked_mul(PAGE_REF_LEN).ok_or_else(corrupt)?;
        let end = start.checked_add(len).ok_or_else(corrupt)?;
        let slice = self.pages.get(start..end).ok_or_else(corrupt)?;
        let mut c = Cursor::new(slice, u64::from(rec.pages_offset));
        (0..n).map(|_| PageRef::decode(&mut c)).collect()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::narrow::BlockEntry;
    use llmobs_core::schema::spans_schema;
    use proptest::prelude::*;

    fn record(column_id: u32, pages_offset: u32) -> DirectoryRecord {
        DirectoryRecord {
            column_id,
            parent: u32::MAX,
            layer: Layer::Narrow,
            flags: 1,
            pages_offset,
            n_pages: 2,
            n_chunks: 5,
            dict_offset: 0,
            dict_len: 0,
            payload_offset: 0,
            payload_len: 0,
            null_count: 3,
        }
    }

    #[test]
    fn postscript_round_trips_and_is_64_bytes() {
        let ps = Postscript {
            hot_offset: 1234,
            hot_len: 56,
            hot_checksum: 0xDEAD_BEEF,
            row_count: 99,
            n_columns: 3,
            version: VERSION,
            flags: 0,
            sort_key_column: 7,
        };
        let mut out = Out(Vec::new());
        ps.encode(&mut out);
        assert_eq!(out.len(), 64);
        let bytes: &[u8; 64] = out.0.as_slice().try_into().unwrap();
        assert_eq!(Postscript::decode(bytes, 0).unwrap(), ps);
    }

    #[test]
    fn postscript_rejects_bad_version_flags_and_magic() {
        let base = Postscript {
            hot_offset: 8,
            hot_len: 0,
            hot_checksum: 0,
            row_count: 0,
            n_columns: 0,
            version: VERSION,
            flags: 0,
            sort_key_column: u32::MAX,
        };
        let mut out = Out(Vec::new());
        base.encode(&mut out);
        let good: [u8; 64] = out.0.clone().try_into().unwrap();

        let mut bad_version = good;
        bad_version[28] = 1;
        assert!(Postscript::decode(&bad_version, 0).is_err());

        let mut bad_flags = good;
        bad_flags[30] = 1;
        assert!(Postscript::decode(&bad_flags, 0).is_err());

        let mut bad_magic = good;
        bad_magic[63] = b'X';
        assert!(Postscript::decode(&bad_magic, 0).is_err());

        let mut bad_reserved = good;
        bad_reserved[40] = 1;
        assert!(Postscript::decode(&bad_reserved, 0).is_err());
    }

    #[test]
    fn postscript_sort_key_column_round_trips_and_reserved_bytes_are_checked() {
        let ps = Postscript {
            hot_offset: 8,
            hot_len: 0,
            hot_checksum: 0,
            row_count: 0,
            n_columns: 0,
            version: VERSION,
            flags: 0,
            sort_key_column: 3,
        };
        let mut out = Out(Vec::new());
        ps.encode(&mut out);
        let good: [u8; 64] = out.0.clone().try_into().unwrap();
        assert_eq!(Postscript::decode(&good, 0).unwrap(), ps);

        // The 24 reserved bytes are at 36..60.
        let mut first_reserved = good;
        first_reserved[36] = 1;
        assert!(Postscript::decode(&first_reserved, 0).is_err());
        let mut last_reserved = good;
        last_reserved[59] = 1;
        assert!(Postscript::decode(&last_reserved, 0).is_err());
    }

    #[test]
    fn directory_record_round_trips_and_is_64_bytes() {
        let rec = record(7, 32);
        let mut out = Out(Vec::new());
        rec.encode(&mut out);
        assert_eq!(out.len(), 64);
        let mut c = Cursor::new(&out.0, 0);
        assert_eq!(DirectoryRecord::decode(&mut c).unwrap(), rec);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn directory_record_rejects_unknown_layer_and_reserved_bytes() {
        let mut out = Out(Vec::new());
        record(0, 0).encode(&mut out);
        let mut bad_layer = out.0.clone();
        bad_layer[8] = 9;
        assert!(DirectoryRecord::decode(&mut Cursor::new(&bad_layer, 0)).is_err());
        let mut bad_reserved = out.0.clone();
        bad_reserved[10] = 1;
        assert!(DirectoryRecord::decode(&mut Cursor::new(&bad_reserved, 0)).is_err());
        let mut bad_reserved2 = out.0.clone();
        bad_reserved2[63] = 1;
        assert!(DirectoryRecord::decode(&mut Cursor::new(&bad_reserved2, 0)).is_err());
    }

    #[test]
    fn page_ref_round_trips_and_is_16_bytes() {
        let page = PageRef {
            first_row: 4096,
            offset: 1 << 40,
            len: 77,
        };
        let mut out = Out(Vec::new());
        page.encode(&mut out);
        assert_eq!(out.len(), 16);
        assert_eq!(PageRef::decode(&mut Cursor::new(&out.0, 0)).unwrap(), page);
    }

    #[test]
    fn hot_tail_round_trips() {
        let hot = HotTail {
            schema: spans_schema(),
            directory: vec![record(0, 0), record(1, 32), record(2, 64)],
            sort_key_entries: vec![0xAB; 2 * BLOCK_ENTRY_LEN],
            sort_key_filter: Vec::new(),
            map_keys: Vec::new(),
            key_filters: Vec::new(),
            pages: vec![0xCD; 4 * PAGE_REF_LEN],
        };
        let mut out = Out(Vec::new());
        hot.encode(&mut out).unwrap();
        let decoded = HotTail::decode(&out.0, 0).unwrap();
        assert_eq!(decoded.schema.as_ref(), hot.schema.as_ref());
        assert_eq!(decoded.directory, hot.directory);
        assert_eq!(decoded.sort_key_entries, hot.sort_key_entries);
        assert_eq!(decoded.sort_key_filter, hot.sort_key_filter);
        assert_eq!(decoded.map_keys, hot.map_keys);
        assert_eq!(decoded.key_filters, hot.key_filters);
        assert_eq!(decoded.pages, hot.pages);
        assert_eq!(decoded.pages_of(&hot.directory[1]).unwrap().len(), 2);
    }

    /// A hot tail whose sections 1, 2, 3, 4, 5 and 7 are valid and empty, and whose section 6
    /// holds `index`.
    fn hot_bytes_with_index(index: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut stream, &spans_schema()).unwrap();
            writer.finish().unwrap();
        }
        let mut out = Out(Vec::new());
        write_section(&mut out, &stream).unwrap();
        for section in [&[][..], &[][..], &[][..], &[][..]] {
            write_section(&mut out, section).unwrap();
        }
        write_section(&mut out, index).unwrap();
        write_section(&mut out, &[]).unwrap();
        out.0
    }

    fn index_entry(column_id: u32) -> Vec<u8> {
        let mut entry = Out(Vec::new());
        entry.u32(column_id);
        entry.u64(4096);
        entry.u32(77);
        entry.0
    }

    #[test]
    fn key_filter_index_round_trips_with_zero_and_two_entries() {
        let empty = hot_bytes_with_index(&0u32.to_le_bytes());
        assert!(HotTail::decode(&empty, 0).unwrap().key_filters.is_empty());

        let mut index = 2u32.to_le_bytes().to_vec();
        index.extend(index_entry(1));
        index.extend(index_entry(3));
        let decoded = HotTail::decode(&hot_bytes_with_index(&index), 0).unwrap();
        assert_eq!(
            decoded.key_filters,
            vec![
                (
                    1,
                    SegmentRef {
                        offset: 4096,
                        len: 77
                    }
                ),
                (
                    3,
                    SegmentRef {
                        offset: 4096,
                        len: 77
                    }
                ),
            ]
        );
    }

    #[test]
    fn key_filter_index_corruptions_are_corrupt() {
        let fields = spans_schema().fields().len() as u32;
        let mut bad_length = 1u32.to_le_bytes().to_vec();
        bad_length.extend(vec![0u8; 15]);
        let mut trailing = 1u32.to_le_bytes().to_vec();
        trailing.extend(index_entry(0));
        trailing.push(0);
        let mut repeated = 2u32.to_le_bytes().to_vec();
        repeated.extend(index_entry(0));
        repeated.extend(index_entry(0));
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("an empty section 6", Vec::new()),
            ("a length of 4 + 15 bytes for one entry", bad_length),
            ("a trailing byte", trailing),
            ("a column id at the schema length", index_entry(fields)),
            ("a repeated column id", repeated),
        ];
        for (name, index) in cases {
            let result = HotTail::decode(&hot_bytes_with_index(&index), 0);
            assert!(
                matches!(
                    result,
                    Err(StrataError::Corrupt {
                        what: "key filter index",
                        ..
                    })
                ),
                "{name} must be a Corrupt error"
            );
        }
    }

    #[test]
    fn hot_tail_rejects_misaligned_sections_and_trailing_bytes() {
        let mut hot = HotTail {
            schema: spans_schema(),
            directory: vec![record(0, 0)],
            sort_key_entries: vec![0; BLOCK_ENTRY_LEN],
            sort_key_filter: Vec::new(),
            map_keys: Vec::new(),
            key_filters: Vec::new(),
            pages: vec![0; PAGE_REF_LEN],
        };
        let mut out = Out(Vec::new());
        hot.encode(&mut out).unwrap();
        let mut trailing = out.0.clone();
        trailing.push(0);
        assert!(HotTail::decode(&trailing, 0).is_err());

        hot.sort_key_entries = vec![0; BLOCK_ENTRY_LEN + 1];
        let mut out = Out(Vec::new());
        hot.encode(&mut out).unwrap();
        assert!(HotTail::decode(&out.0, 0).is_err());
    }

    #[test]
    fn hot_tail_decode_never_panics_with_random_sections() {
        // Section 1 is valid, because the checksum in file.rs guards it. Only the
        // other sections are random.
        let mut stream = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut stream, &spans_schema()).unwrap();
            writer.finish().unwrap();
        }
        proptest!(|(
            directory in proptest::collection::vec(any::<u8>(), 0..256),
            sort_key in proptest::collection::vec(any::<u8>(), 0..256),
            filter in proptest::collection::vec(any::<u8>(), 0..64),
            keys in proptest::collection::vec(any::<u8>(), 0..64),
            index in proptest::collection::vec(any::<u8>(), 0..64),
            pages in proptest::collection::vec(any::<u8>(), 0..256),
        )| {
            let mut out = Out(Vec::new());
            write_section(&mut out, &stream).unwrap();
            write_section(&mut out, &directory).unwrap();
            write_section(&mut out, &sort_key).unwrap();
            write_section(&mut out, &filter).unwrap();
            write_section(&mut out, &keys).unwrap();
            write_section(&mut out, &index).unwrap();
            write_section(&mut out, &pages).unwrap();
            let _ = HotTail::decode(&out.0, 0);
        });
    }

    proptest! {
        #[test]
        fn random_block_entries_never_panic(bytes in proptest::collection::vec(any::<u8>(), 56..=56)) {
            let _ = BlockEntry::decode(&mut Cursor::new(&bytes, 0));
        }

        #[test]
        fn random_page_lists_never_panic(
            pages in proptest::collection::vec(any::<u8>(), 0..256),
            pages_offset in 0u32..300,
            n_pages in 0u32..20,
        ) {
            let hot = HotTail {
                schema: spans_schema(),
                directory: vec![record(0, pages_offset)],
                sort_key_entries: Vec::new(),
                sort_key_filter: Vec::new(),
                map_keys: Vec::new(),
                key_filters: Vec::new(),
                pages,
            };
            let rec = DirectoryRecord { n_pages, ..record(0, pages_offset) };
            let _ = hot.pages_of(&rec);
        }
    }
}
