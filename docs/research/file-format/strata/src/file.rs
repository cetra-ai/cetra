//! Opening a Strata file from an object store with one suffix read (S1 open).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use arrow::array::{Array, FixedSizeBinaryArray};
use arrow::datatypes::{DataType, Field, SchemaRef};
use futures::future::try_join_all;
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{
    Cursor, HEADER_LEN, MAX_PAGE_ENTRIES, MAX_ROWS, POSTSCRIPT_LEN, SEGMENT_HEADER_LEN, checksum32,
};
use crate::hot::{DirectoryRecord, HotTail, Layer, Postscript};
use crate::index::{KeyFilter, merge_rows, zone_may_hold};
use crate::narrow::{BlockEntry, decode_chunk, is_utf8_map};
use crate::segment::{Kind, SegmentRef, decode_segment};

#[cfg(test)]
#[path = "../tests/common/counting.rs"]
mod counting;

/// Checks that a segment of `len` bytes at `offset` lies after the file header and before the
/// hot tail, and is at least a segment header long. Returns the end offset of the segment.
/// Every segment is checked this way before it is fetched.
fn segment_end(offset: u64, len: u32, hot_offset: u64) -> Result<u64> {
    let corrupt = || StrataError::Corrupt {
        what: "segment range",
        offset,
    };
    let end = offset.checked_add(u64::from(len)).ok_or_else(corrupt)?;
    if offset >= HEADER_LEN && u64::from(len) >= SEGMENT_HEADER_LEN as u64 && end <= hot_offset {
        Ok(end)
    } else {
        Err(corrupt())
    }
}

/// Decodes the sort-key block table (hot-tail section 3) of the sort-key column `sort`. The
/// entries are contiguous from row 0, cover `row_count` rows, match the column's chunk count
/// and have in-range segments.
fn read_sort_key_entries(
    hot: &HotTail,
    sort: usize,
    row_count: u64,
    hot_offset: u64,
) -> Result<Vec<BlockEntry>> {
    let corrupt = || StrataError::Corrupt {
        what: "sort-key block table",
        offset: hot_offset,
    };
    let n_chunks = hot.directory.get(sort).ok_or_else(corrupt)?.n_chunks;
    let mut c = Cursor::new(&hot.sort_key_entries, hot_offset);
    let mut entries = Vec::new();
    let mut next_row = 0u64;
    while c.remaining() > 0 {
        let entry = BlockEntry::decode(&mut c)?;
        if u64::from(entry.first_row) != next_row || entry.n_rows == 0 {
            return Err(corrupt());
        }
        if entry.segment_len > 0 {
            segment_end(entry.segment_offset, entry.segment_len, hot_offset)
                .map_err(|_| corrupt())?;
        }
        next_row = next_row
            .checked_add(u64::from(entry.n_rows))
            .ok_or_else(corrupt)?;
        entries.push(entry);
    }
    if next_row != row_count || entries.len() != n_chunks as usize {
        return Err(corrupt());
    }
    Ok(entries)
}

/// Checks the map records and section 5 against each other. Every `MapParent` is named once,
/// every record past the schema is named once, and every name is a parent, a child or a
/// residual of the map that names it. Records of the schema that are not maps are named by
/// nothing.
fn check_map_layout(hot: &HotTail, hot_offset: u64) -> Result<()> {
    let corrupt = || StrataError::Corrupt {
        what: "map layout",
        offset: hot_offset,
    };
    let records = &hot.directory;
    let n_fields = hot.schema.fields().len();
    let index = |id: u32| usize::try_from(id).ok().filter(|i| *i < records.len());
    // Whether the record at `at` has `layer` and names `parent`.
    let has_parent = |at: usize, layer: Layer, parent: u32| {
        records
            .get(at)
            .is_some_and(|r| r.layer == layer && r.parent == parent)
    };

    let mut named = vec![0usize; records.len()];
    for layout in &hot.maps {
        let map_at = index(layout.map).ok_or_else(corrupt)?;
        if records.get(map_at).map(|r| r.layer) != Some(Layer::MapParent) {
            return Err(corrupt());
        }
        let residual_at = index(layout.residual).ok_or_else(corrupt)?;
        if !has_parent(residual_at, Layer::Residual, layout.map) {
            return Err(corrupt());
        }
        *named.get_mut(map_at).ok_or_else(corrupt)? += 1;
        *named.get_mut(residual_at).ok_or_else(corrupt)? += 1;
        for key in &layout.keys {
            let child_at = index(key.child).ok_or_else(corrupt)?;
            if !has_parent(child_at, Layer::MapChild, layout.map) {
                return Err(corrupt());
            }
            *named.get_mut(child_at).ok_or_else(corrupt)? += 1;
        }
    }
    for (i, record) in records.iter().enumerate() {
        let expected = usize::from(record.layer == Layer::MapParent || i >= n_fields);
        if named.get(i) != Some(&expected) {
            return Err(corrupt());
        }
    }
    Ok(())
}

/// An open Strata file: its postscript and decoded hot tail, plus the store to read
/// segments from.
pub struct StrataFile {
    store: Arc<dyn ObjectStore>,
    path: Path,
    size: u64,
    postscript: Postscript,
    pub(crate) hot: HotTail,
    /// Index of the first sort-key column, or `None` when the file has no sort key.
    sort_key: Option<usize>,
    /// The sort-key filter (hot-tail section 4), decoded at open. `None` when the section is
    /// empty.
    sort_key_filter: Option<KeyFilter>,
    /// The sort-key block table (hot-tail section 3), decoded at open, in row order.
    sort_key_entries: Vec<BlockEntry>,
    /// Key filters read on first use, by column id.
    key_filters: Mutex<HashMap<u32, Arc<KeyFilter>>>,
    /// Block-table entries of each column, read on first use.
    blocks: Mutex<HashMap<usize, Arc<Vec<BlockEntry>>>>,
}

impl StrataFile {
    /// Opens the file at `path`.
    ///
    /// One suffix read covers the postscript and, when `hot_len_hint` is given, the
    /// whole hot tail. Without a hint the suffix is `config.open_default_suffix`. A
    /// longer hot tail costs one more ranged read.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        path: Path,
        hot_len_hint: Option<u32>,
        config: &Config,
    ) -> Result<StrataFile> {
        let n = hot_len_hint
            .map(|h| u64::from(h) + POSTSCRIPT_LEN)
            .unwrap_or(config.open_default_suffix);
        let read = store
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Suffix(n)),
                    ..Default::default()
                },
            )
            .await?;
        let size = read.meta.size;
        let start = read.range.start;
        let tail = read.bytes().await?;

        let too_small = || StrataError::Corrupt {
            what: "file too small",
            offset: 0,
        };
        if size < HEADER_LEN + POSTSCRIPT_LEN {
            return Err(too_small());
        }
        let ps_start = tail
            .len()
            .checked_sub(POSTSCRIPT_LEN as usize)
            .ok_or_else(too_small)?;
        let ps: &[u8; 64] = tail
            .get(ps_start..)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(too_small)?;
        let postscript = Postscript::decode(ps, size.saturating_sub(POSTSCRIPT_LEN))?;
        if postscript.row_count > MAX_ROWS {
            return Err(StrataError::Corrupt {
                what: "row count",
                offset: size.saturating_sub(POSTSCRIPT_LEN),
            });
        }

        if postscript.hot_offset < HEADER_LEN {
            return Err(StrataError::Corrupt {
                what: "hot offset",
                offset: postscript.hot_offset,
            });
        }
        let hot_end = postscript
            .hot_offset
            .checked_add(u64::from(postscript.hot_len))
            .and_then(|v| v.checked_add(POSTSCRIPT_LEN));
        if hot_end != Some(size) {
            return Err(StrataError::Corrupt {
                what: "hot tail length",
                offset: postscript.hot_offset,
            });
        }

        let hot_corrupt = || StrataError::Corrupt {
            what: "hot tail range",
            offset: postscript.hot_offset,
        };
        let hot: Vec<u8> = if postscript.hot_offset < start {
            let head = store.get_range(&path, postscript.hot_offset..start).await?;
            let body = tail.get(..ps_start).ok_or_else(hot_corrupt)?;
            let mut joined = Vec::with_capacity(head.len() + body.len());
            joined.extend_from_slice(&head);
            joined.extend_from_slice(body);
            joined
        } else {
            let skip = postscript
                .hot_offset
                .checked_sub(start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(hot_corrupt)?;
            tail.get(skip..ps_start).ok_or_else(hot_corrupt)?.to_vec()
        };

        if checksum32(&[hot.as_slice()]) != postscript.hot_checksum {
            return Err(StrataError::Corrupt {
                what: "hot checksum",
                offset: postscript.hot_offset,
            });
        }
        let hot = HotTail::decode(&hot, postscript.hot_offset)?;
        if hot.directory.len() != postscript.n_columns as usize {
            return Err(StrataError::Corrupt {
                what: "directory count",
                offset: postscript.hot_offset,
            });
        }
        // Every schema column has a record, in order. Records past the schema are hidden
        // map children (M3); they carry their own ids, which are checked the same way.
        let n_fields = hot.schema.fields().len();
        if hot.directory.len() < n_fields {
            return Err(StrataError::Corrupt {
                what: "directory count",
                offset: postscript.hot_offset,
            });
        }
        for (i, record) in hot.directory.iter().enumerate() {
            if usize::try_from(record.column_id).ok() != Some(i) {
                return Err(StrataError::Corrupt {
                    what: "directory column id",
                    offset: postscript.hot_offset,
                });
            }
            // A schema column is narrow, a payload or a map parent; a map parent must be a
            // shredded-map type. Past the schema are map children and residuals only.
            let layer_ok = if i < n_fields {
                matches!(
                    record.layer,
                    Layer::Narrow | Layer::Payload | Layer::MapParent
                ) && (record.layer != Layer::MapParent
                    || hot
                        .schema
                        .fields()
                        .get(i)
                        .is_some_and(|field| is_utf8_map(field.data_type())))
            } else {
                matches!(record.layer, Layer::MapChild | Layer::Residual)
            };
            if !layer_ok {
                return Err(StrataError::Corrupt {
                    what: "directory layer",
                    offset: postscript.hot_offset,
                });
            }
        }
        check_map_layout(&hot, postscript.hot_offset)?;
        // Checked after the directory loop, so the id and layer errors above come first.
        let bad_sort_key = || StrataError::Corrupt {
            what: "sort key column",
            offset: postscript.hot_offset,
        };
        let sort_key = match postscript.sort_key_column {
            u32::MAX => None,
            column => {
                let index = usize::try_from(column).map_err(|_| bad_sort_key())?;
                match hot.directory.get(index) {
                    Some(record) if index < n_fields && record.layer == Layer::Narrow => {
                        Some(index)
                    }
                    _ => return Err(bad_sort_key()),
                }
            }
        };
        let sort_key_filter = if hot.sort_key_filter.is_empty() {
            None
        } else {
            Some(KeyFilter::decode(
                &hot.sort_key_filter,
                postscript.hot_offset,
            )?)
        };
        let sort_key_entries = match sort_key {
            Some(sort) => {
                read_sort_key_entries(&hot, sort, postscript.row_count, postscript.hot_offset)?
            }
            None if hot.sort_key_entries.is_empty() => Vec::new(),
            None => {
                return Err(StrataError::Corrupt {
                    what: "sort-key block table",
                    offset: postscript.hot_offset,
                });
            }
        };
        Ok(StrataFile {
            store,
            path,
            size,
            postscript,
            hot,
            sort_key,
            sort_key_filter,
            sort_key_entries,
            key_filters: Mutex::new(HashMap::new()),
            blocks: Mutex::new(HashMap::new()),
        })
    }

    /// The table schema.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.hot.schema)
    }

    /// Number of rows in the file.
    pub fn row_count(&self) -> u64 {
        self.postscript.row_count
    }

    /// Size of the file in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Length of the hot tail in bytes.
    pub fn hot_len(&self) -> u32 {
        self.postscript.hot_len
    }

    /// The column directory.
    pub fn directory(&self) -> &[DirectoryRecord] {
        &self.hot.directory
    }

    /// The field that directory record `column` is read as: the schema field of a schema
    /// column, a nullable unnamed Utf8 for a map child, and the parent's field for a residual.
    pub(crate) fn field_of(&self, column: usize) -> Result<Field> {
        let schema = &self.hot.schema;
        if let Some(field) = schema.fields().get(column) {
            return Ok(field.as_ref().clone());
        }
        let record = self
            .hot
            .directory
            .get(column)
            .ok_or_else(|| StrataError::Schema {
                reason: format!("no column {column}"),
            })?;
        let corrupt = || StrataError::Corrupt {
            what: "map layout",
            offset: self.postscript.hot_offset,
        };
        match record.layer {
            Layer::MapChild => Ok(Field::new("", DataType::Utf8, true)),
            Layer::Residual => usize::try_from(record.parent)
                .ok()
                .and_then(|parent| schema.fields().get(parent))
                .map(|field| field.as_ref().clone())
                .ok_or_else(corrupt),
            _ => Err(corrupt()),
        }
    }

    /// Reads and checks one segment of the given kind.
    pub async fn read_segment(&self, r: &SegmentRef, kind: Kind) -> Result<Vec<u8>> {
        let end = segment_end(r.offset, r.len, self.postscript.hot_offset)?;
        let bytes = self.store.get_range(&self.path, r.offset..end).await?;
        decode_segment(&bytes, r.offset, kind)
    }

    /// The block-table entries of column `column`, in row order. They are read from the
    /// column's pages on first use and cached.
    pub async fn block_entries(&self, column: usize) -> Result<Arc<Vec<BlockEntry>>> {
        let cached = self
            .blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&column)
            .cloned();
        if let Some(entries) = cached {
            return Ok(entries);
        }

        let record = self
            .hot
            .directory
            .get(column)
            .ok_or_else(|| StrataError::Schema {
                reason: format!("no column {column}"),
            })?;
        if record.layer == Layer::Payload {
            return Ok(Arc::new(Vec::new()));
        }
        let hot_offset = self.postscript.hot_offset;
        let pages = self.hot.pages_of(record)?;
        for page in &pages {
            segment_end(page.offset, page.len, hot_offset)?;
        }
        let bodies = try_join_all(pages.iter().map(|page| async move {
            let segment = SegmentRef {
                offset: page.offset,
                len: page.len,
            };
            self.read_segment(&segment, Kind::BlockTablePage).await
        }))
        .await?;

        let corrupt_page = |offset: u64| StrataError::Corrupt {
            what: "block page",
            offset,
        };
        let mut entries = Vec::new();
        let mut next_row = 0u64;
        for (page, body) in pages.iter().zip(&bodies) {
            // A page holds between 1 and MAX_PAGE_ENTRIES entries, and its first row is the
            // first row of its first entry.
            let mut c = Cursor::new(body, page.offset);
            let mut page_entries = 0usize;
            while c.remaining() > 0 {
                let entry = BlockEntry::decode(&mut c)?;
                if page_entries == 0 && entry.first_row != page.first_row {
                    return Err(corrupt_page(page.offset));
                }
                page_entries += 1;
                if page_entries > MAX_PAGE_ENTRIES {
                    return Err(corrupt_page(page.offset));
                }
                if u64::from(entry.first_row) != next_row || entry.n_rows == 0 {
                    return Err(StrataError::Corrupt {
                        what: "block row order",
                        offset: page.offset,
                    });
                }
                if entry.segment_len > 0 {
                    segment_end(entry.segment_offset, entry.segment_len, hot_offset)?;
                }
                next_row += u64::from(entry.n_rows);
                entries.push(entry);
            }
            if page_entries == 0 {
                return Err(corrupt_page(page.offset));
            }
        }
        if next_row != self.row_count() {
            return Err(StrataError::Corrupt {
                what: "block row count",
                offset: self.postscript.hot_offset,
            });
        }
        if entries.len() != record.n_chunks as usize {
            return Err(StrataError::Corrupt {
                what: "block chunk count",
                offset: self.postscript.hot_offset,
            });
        }

        let entries = Arc::new(entries);
        self.blocks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(column, Arc::clone(&entries));
        Ok(entries)
    }

    /// Whether `value` may occur in column `column`. `Ok(None)` means the column has no
    /// filter. `Some(false)` is a proof of absence. Integer and timestamp values are passed as
    /// their 8-byte little-endian `i64`.
    pub async fn may_contain(&self, column: &str, value: &[u8]) -> Result<Option<bool>> {
        let index = self
            .hot
            .schema
            .fields()
            .iter()
            .position(|field| field.name() == column)
            .ok_or_else(|| StrataError::Schema {
                reason: format!("no column {column}"),
            })?;
        if Some(index) == self.sort_key {
            return Ok(self
                .sort_key_filter
                .as_ref()
                .and_then(|filter| filter.may_contain(value)));
        }
        let Some(&(column_id, segment)) = self
            .hot
            .key_filters
            .iter()
            .find(|(id, _)| usize::try_from(*id).ok() == Some(index))
        else {
            return Ok(None);
        };
        let cached = self
            .key_filters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&column_id)
            .cloned();
        let filter = match cached {
            Some(filter) => filter,
            None => {
                let bytes = self.read_segment(&segment, Kind::KeyFilter).await?;
                let filter = Arc::new(KeyFilter::decode(&bytes, segment.offset)?);
                self.key_filters
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(column_id, Arc::clone(&filter));
                filter
            }
        };
        Ok(filter.may_contain(value))
    }

    /// The row ranges whose first sort-key column equals `key`, in row order, with adjacent
    /// rows merged. Only the sort-key chunks whose zones may hold `key` are read.
    ///
    /// The first sort-key column must be FixedSizeBinary of 1 to 16 bytes, and `key` must have
    /// that width. Otherwise this is `Unsupported`.
    pub async fn lookup(&self, key: &[u8]) -> Result<Vec<Range<u32>>> {
        let unsupported = || StrataError::Unsupported {
            what: "lookup on this sort key".to_string(),
        };
        let sort = self.sort_key.ok_or_else(unsupported)?;
        let field = self.hot.schema.fields().get(sort).ok_or_else(unsupported)?;
        let width = match field.data_type() {
            DataType::FixedSizeBinary(w) => usize::try_from(*w).ok(),
            _ => None,
        };
        if !matches!(width, Some(w) if (1..=16).contains(&w) && w == key.len()) {
            return Err(unsupported());
        }

        if self
            .sort_key_filter
            .as_ref()
            .and_then(|filter| filter.may_contain(key))
            == Some(false)
        {
            return Ok(Vec::new());
        }
        let candidates: Vec<BlockEntry> = self
            .sort_key_entries
            .iter()
            .filter(|entry| zone_may_hold(entry, key))
            .copied()
            .collect();
        let hot_offset = self.postscript.hot_offset;
        let corrupt = || StrataError::Corrupt {
            what: "sort-key block table",
            offset: hot_offset,
        };

        let bodies = try_join_all(candidates.iter().map(|entry| async move {
            if entry.segment_len == 0 {
                return Ok::<Option<Vec<u8>>, StrataError>(None);
            }
            let segment = SegmentRef {
                offset: entry.segment_offset,
                len: entry.segment_len,
            };
            self.read_segment(&segment, Kind::NarrowChunk)
                .await
                .map(Some)
        }))
        .await?;

        let mut rows = Vec::new();
        for (entry, body) in candidates.iter().zip(&bodies) {
            let array = decode_chunk(entry, body.as_deref(), field.as_ref(), None)?;
            let column = array
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .ok_or_else(corrupt)?;
            for i in 0..column.len() {
                if column.is_valid(i) && column.value(i) == key {
                    let row = u32::try_from(i)
                        .ok()
                        .and_then(|i| entry.first_row.checked_add(i))
                        .ok_or_else(corrupt)?;
                    if u64::from(row) >= self.row_count() {
                        return Err(corrupt());
                    }
                    rows.push(row);
                }
            }
        }
        Ok(merge_rows(&rows))
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
    use crate::format::{PAGE_REF_LEN, SEGMENT_HEADER_LEN};
    use crate::testgen::{assert_same, batch, schema as batch_schema, wide_batch, wide_schema};
    use crate::writer::StrataWriter;
    use arrow::array::{AsArray, FixedSizeBinaryArray, RecordBatch, StringArray};
    use arrow::buffer::Buffer;
    use arrow::datatypes::{Field, Schema};
    use counting::Counting;
    use llmobs_core::schema::spans_schema;
    use object_store::PutPayload;
    use object_store::memory::InMemory;
    use proptest::prelude::*;
    use xxhash_rust::xxh3::xxh3_128;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A file of the spans schema with zero rows.
    fn write_empty_file(schema: SchemaRef) -> Vec<u8> {
        StrataWriter::new(schema, &["trace_id", "start_time"], Config::default())
            .unwrap()
            .finish()
            .unwrap()
            .0
    }

    fn path() -> Path {
        Path::from("table.strata")
    }

    async fn put(store: &dyn ObjectStore, bytes: Vec<u8>) {
        store.put(&path(), PutPayload::from(bytes)).await.unwrap();
    }

    fn is_corrupt<T>(result: &Result<T>) -> bool {
        matches!(result, Err(StrataError::Corrupt { .. }))
    }

    #[test]
    fn empty_file_opens() {
        runtime().block_on(async {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            put(store.as_ref(), write_empty_file(spans_schema())).await;
            let file = StrataFile::open(store, path(), None, &Config::default())
                .await
                .unwrap();
            assert_eq!(file.row_count(), 0);
            assert_eq!(file.schema().as_ref(), spans_schema().as_ref());
            // Every column has a directory record, even with no chunks. The three
            // `Map<Utf8, Utf8>` columns of the spans schema each add a residual, and no key.
            assert_eq!(file.directory().len(), spans_schema().fields().len() + 3);
            assert!(file.directory().iter().all(|r| r.n_chunks == 0));
        });
    }

    #[test]
    fn row_count_past_the_limit_is_corrupt() {
        runtime().block_on(async {
            let mut bytes = write_empty_file(spans_schema());
            let ps_at = bytes.len() - 64;
            bytes[ps_at + 16..ps_at + 24].copy_from_slice(&(1u64 << 32).to_le_bytes());
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            put(store.as_ref(), bytes).await;
            let result = StrataFile::open(store, path(), None, &Config::default()).await;
            assert!(matches!(
                result,
                Err(StrataError::Corrupt {
                    what: "row count",
                    ..
                })
            ));
        });
    }

    #[test]
    fn constant_sort_key_chunk_answers_by_its_zone() {
        runtime().block_on(async {
            let n = 1_000usize;
            let schema = Arc::new(Schema::new(vec![
                Field::new("trace_id", DataType::FixedSizeBinary(16), false),
                Field::new("n", DataType::Int64, false),
            ]));
            let ids = FixedSizeBinaryArray::try_new(16, Buffer::from_vec(vec![7u8; 16 * n]), None)
                .unwrap();
            let values = arrow::array::Int64Array::from_iter_values(0..n as i64);
            let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(values)])
                .unwrap();
            let mut writer = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
            writer.write(&batch).unwrap();
            let (bytes, _) = writer.finish().unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            put(store.as_ref(), bytes).await;
            let file = StrataFile::open(store, path(), None, &Config::default())
                .await
                .unwrap();
            // A key outside the single constant chunk's zone finds nothing.
            assert!(file.lookup(&[8u8; 16]).await.unwrap().is_empty());
            // The key of the chunk finds every row, as one range.
            assert_eq!(file.lookup(&[7u8; 16]).await.unwrap(), vec![0..n as u32]);
        });
    }

    #[test]
    fn exact_hint_makes_one_get() {
        runtime().block_on(async {
            let counting = Arc::new(Counting::new());
            let store: Arc<dyn ObjectStore> = counting.clone();
            put(store.as_ref(), write_empty_file(spans_schema())).await;
            let hot_len = StrataFile::open(store.clone(), path(), None, &Config::default())
                .await
                .unwrap()
                .hot_len();
            let before = counting.gets();
            StrataFile::open(store, path(), Some(hot_len), &Config::default())
                .await
                .unwrap();
            assert_eq!(counting.gets() - before, 1);
        });
    }

    #[test]
    fn short_hint_makes_two_gets() {
        runtime().block_on(async {
            let counting = Arc::new(Counting::new());
            let store: Arc<dyn ObjectStore> = counting.clone();
            put(store.as_ref(), write_empty_file(spans_schema())).await;
            let before = counting.gets();
            StrataFile::open(store, path(), Some(1), &Config::default())
                .await
                .unwrap();
            assert_eq!(counting.gets() - before, 2);
        });
    }

    #[test]
    fn no_hint_makes_one_get() {
        runtime().block_on(async {
            let counting = Arc::new(Counting::new());
            let store: Arc<dyn ObjectStore> = counting.clone();
            put(store.as_ref(), write_empty_file(spans_schema())).await;
            let before = counting.gets();
            StrataFile::open(store, path(), None, &Config::default())
                .await
                .unwrap();
            assert_eq!(counting.gets() - before, 1);
        });
    }

    #[test]
    fn corruptions_are_corrupt_errors() {
        runtime().block_on(async {
            let good = write_empty_file(spans_schema());
            let ps_at = good.len() - 64;
            let hot_start = u64::from_le_bytes(good[ps_at..ps_at + 8].try_into().unwrap()) as usize;

            let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
            cases.push(("truncated by one byte", good[..good.len() - 1].to_vec()));
            let mut last4 = good.clone();
            let n = last4.len();
            last4[n - 1] ^= 0xFF;
            cases.push(("last four bytes changed", last4));
            let mut inside = good.clone();
            inside[hot_start + 4] ^= 0x01;
            cases.push(("hot tail byte flipped", inside));
            let mut patched = good.clone();
            patched[ps_at..ps_at + 8].copy_from_slice(&(hot_start as u64 + 1).to_le_bytes());
            cases.push(("hot_offset patched", patched));

            for (name, bytes) in cases {
                let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                put(store.as_ref(), bytes).await;
                let result = StrataFile::open(store, path(), None, &Config::default()).await;
                assert!(is_corrupt(&result), "{name} must be a Corrupt error");
            }
        });
    }

    #[test]
    fn random_bytes_never_panic() {
        proptest!(|(bytes in proptest::collection::vec(any::<u8>(), 0..2048))| {
            runtime().block_on(async {
                let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                put(store.as_ref(), bytes).await;
                let _ = StrataFile::open(store, path(), None, &Config::default()).await;
            });
        });
    }

    #[test]
    fn mutated_valid_file_never_panics() {
        let good = write_empty_file(spans_schema());
        proptest!(|(
            flip in any::<bool>(),
            index in 0usize..10_000,
            value in 1u8..=255,
        )| {
            let mut bytes = good.clone();
            let at = index % bytes.len();
            if flip {
                bytes[at] ^= value;
            } else {
                bytes.truncate(at);
            }
            runtime().block_on(async {
                let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                put(store.as_ref(), bytes).await;
                let _ = StrataFile::open(store, path(), None, &Config::default()).await;
            });
        });
    }

    /// The offset of the hot tail, read from the postscript at the end of `bytes`.
    fn hot_offset_of(bytes: &[u8]) -> u64 {
        let ps_at = bytes.len() - 64;
        u64::from_le_bytes(bytes[ps_at..ps_at + 8].try_into().unwrap())
    }

    /// Rewrites the hot checksum in the postscript, after the hot tail was edited.
    fn refresh_hot_checksum(bytes: &mut [u8]) {
        let hot = hot_offset_of(bytes) as usize;
        let ps_at = bytes.len() - 64;
        let sum = checksum32(&[&bytes[hot..ps_at]]);
        bytes[ps_at + 12..ps_at + 16].copy_from_slice(&sum.to_le_bytes());
    }

    /// The offset of the first column directory record. Section 1 (the schema) and section 2
    /// (the directory) follow the hot tail's start, each behind its `u32` length.
    fn directory_at(bytes: &[u8]) -> usize {
        let hot = hot_offset_of(bytes) as usize;
        let schema_len = u32::from_le_bytes(bytes[hot..hot + 4].try_into().unwrap()) as usize;
        hot + 4 + schema_len + 4
    }

    /// `batch(20_000)` written with one block-table entry per page, so the hot tail is long.
    fn populated() -> (Vec<u8>, crate::writer::FileSummary) {
        let config = Config {
            page_max_entries: 1,
            ..Config::default()
        };
        let mut w = StrataWriter::new(batch_schema(), &["trace_id", "start_time"], config).unwrap();
        w.write(&batch(20_000)).unwrap();
        w.finish().unwrap()
    }

    async fn open_patched(bytes: Vec<u8>) -> Result<StrataFile> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        put(store.as_ref(), bytes).await;
        StrataFile::open(store, path(), None, &Config::default()).await
    }

    async fn assert_columns_round_trip(file: &StrataFile) {
        let full = batch(20_000);
        for column in 0..full.num_columns() {
            let decoded = crate::reader::read_column(file, column).await.unwrap();
            assert_same(decoded.as_ref(), full.column(column).as_ref());
        }
    }

    #[test]
    fn populated_file_opens_with_one_or_two_gets() {
        runtime().block_on(async {
            let (bytes, summary) = populated();
            assert!(u64::from(summary.hot_len) + POSTSCRIPT_LEN > 1024);
            let counting = Arc::new(Counting::new());
            let store: Arc<dyn ObjectStore> = counting.clone();
            put(store.as_ref(), bytes).await;

            // Without a hint, the suffix is too short for the hot tail: one more read.
            let short = Config {
                open_default_suffix: 1024,
                ..Config::default()
            };
            let before = counting.gets();
            let file = StrataFile::open(store.clone(), path(), None, &short)
                .await
                .unwrap();
            assert_eq!(counting.gets() - before, 2);
            assert_columns_round_trip(&file).await;

            // With the exact hint, one read covers everything.
            let before = counting.gets();
            let file = StrataFile::open(store, path(), Some(summary.hot_len), &Config::default())
                .await
                .unwrap();
            assert_eq!(counting.gets() - before, 1);
            assert_columns_round_trip(&file).await;
        });
    }

    #[test]
    fn block_page_first_row_that_disagrees_with_its_entries_is_corrupt() {
        runtime().block_on(async {
            let (mut bytes, _) = populated();
            // The last page reference is the last 16 bytes before the postscript: it belongs
            // to the last column.
            let at = bytes.len() - 64 - PAGE_REF_LEN;
            bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            refresh_hot_checksum(&mut bytes);
            let file = open_patched(bytes).await.unwrap();
            let last = file.directory().len() - 1;
            assert!(is_corrupt(&file.block_entries(last).await));
        });
    }

    #[test]
    fn block_page_offset_inside_the_hot_tail_is_corrupt() {
        runtime().block_on(async {
            let (mut bytes, _) = populated();
            let at = bytes.len() - 64 - PAGE_REF_LEN;
            let hot = hot_offset_of(&bytes);
            bytes[at + 4..at + 12].copy_from_slice(&(hot + 1).to_le_bytes());
            refresh_hot_checksum(&mut bytes);
            let file = open_patched(bytes).await.unwrap();
            let last = file.directory().len() - 1;
            assert!(is_corrupt(&file.block_entries(last).await));
        });
    }

    #[test]
    fn directory_ids_layers_and_dictionary_flags_are_checked() {
        runtime().block_on(async {
            let good = populated().0;
            let dir = directory_at(&good);

            // Record 0 names column 7 instead of 0.
            let mut bytes = good.clone();
            bytes[dir..dir + 4].copy_from_slice(&7u32.to_le_bytes());
            refresh_hot_checksum(&mut bytes);
            assert!(matches!(
                open_patched(bytes).await,
                Err(StrataError::Corrupt {
                    what: "directory column id",
                    ..
                })
            ));

            // Record 0 has the Residual layer, which a schema column may not have.
            let mut bytes = good.clone();
            bytes[dir + 8] = 4;
            refresh_hot_checksum(&mut bytes);
            assert!(matches!(
                open_patched(bytes).await,
                Err(StrataError::Corrupt {
                    what: "directory layer",
                    ..
                })
            ));

            // Record 2 (input_tokens, no dictionary) claims one anyway.
            let mut bytes = good.clone();
            bytes[dir + 64 * 2 + 9] |= 1;
            refresh_hot_checksum(&mut bytes);
            let file = open_patched(bytes).await.unwrap();
            assert!(matches!(
                crate::reader::read_column(&file, 2).await,
                Err(StrataError::Corrupt {
                    what: "dictionary flag",
                    ..
                })
            ));
        });
    }

    /// `wide_batch(20_000)`, written with the default configuration.
    fn wide_file() -> Vec<u8> {
        let mut w = StrataWriter::new(
            wide_schema(),
            &["trace_id", "start_time"],
            Config::default(),
        )
        .unwrap();
        w.write(&wide_batch(20_000)).unwrap();
        w.finish().unwrap().0
    }

    #[test]
    fn shredded_map_layout_is_written_and_read_back() {
        runtime().block_on(async {
            let file = open_patched(wide_file()).await.unwrap();
            // `attrs` is column 6. Its children are 15 to 17 (`k0`, `k1`, `k2`), its residual 18.
            assert_eq!(file.directory().len(), 19);
            assert_eq!(file.directory()[6].layer, Layer::MapParent);
            for at in 15..18 {
                let child = &file.directory()[at];
                assert_eq!((child.layer, child.parent), (Layer::MapChild, 6));
            }
            assert_eq!(file.directory()[18].layer, Layer::Residual);
            assert_eq!(file.directory()[18].parent, 6);
            assert_eq!(file.hot.maps.len(), 1);
            let layout = &file.hot.maps[0];
            assert_eq!((layout.map, layout.residual), (6, 18));
            let children: Vec<u32> = layout.keys.iter().map(|key| key.child).collect();
            assert_eq!(children, vec![15, 16, 17]);
            let keys: Vec<&[u8]> = layout.keys.iter().map(|key| key.key.as_slice()).collect();
            assert_eq!(keys, vec![&b"k0"[..], &b"k1"[..], &b"k2"[..]]);

            let rows = wide_batch(20_000);
            let decoded = crate::reader::read_column(&file, 6).await.unwrap();
            assert_same(decoded.as_ref(), rows.column(6).as_ref());
            assert!(matches!(
                crate::reader::read_column(&file, 15).await,
                Err(StrataError::Schema { .. })
            ));
        });
    }

    #[test]
    fn map_layout_patches_are_corrupt() {
        runtime().block_on(async {
            let good = wide_file();
            let hot = hot_offset_of(&good) as usize;
            let sections = section_bounds(&good[hot..good.len() - 64]);
            // Section 5 is the fifth section; its body starts at `map_section`.
            let map_section = hot + sections[4].start;
            let dir = directory_at(&good);
            let mut patches = Vec::new();

            // Section 5 names column 0 as a map, and column 0 is not a map parent.
            let mut bytes = good.clone();
            let at = map_section + 4;
            bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
            patches.push(bytes);

            // Child 15 names column 0 as its parent, not map 6.
            let mut bytes = good.clone();
            let at = dir + 64 * 15 + 4;
            bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
            patches.push(bytes);

            // The last byte of key `k0` becomes `9`, so the keys are no longer increasing.
            let mut bytes = good.clone();
            let section = &good[map_section..map_section + sections[4].len()];
            let pos = section.windows(4).position(|w| w == b"\x02\x00k0").unwrap();
            bytes[map_section + pos + 3] = b'9';
            patches.push(bytes);

            for mut bytes in patches {
                refresh_hot_checksum(&mut bytes);
                assert!(matches!(
                    open_patched(bytes).await,
                    Err(StrataError::Corrupt {
                        what: "map layout",
                        ..
                    })
                ));
            }
        });
    }

    /// The body ranges of the seven `u32`-prefixed sections of `hot`, a hot tail. Each range
    /// excludes its length prefix and is relative to the start of `hot`.
    fn section_bounds(hot: &[u8]) -> [Range<usize>; 7] {
        let mut at = 0usize;
        std::array::from_fn(|_| {
            let len = u32::from_le_bytes(hot[at..at + 4].try_into().unwrap()) as usize;
            let range = at + 4..at + 4 + len;
            at = range.end;
            range
        })
    }

    /// `batch(20_000)` written with the default configuration.
    fn batch_file() -> Vec<u8> {
        let mut w = StrataWriter::new(
            batch_schema(),
            &["trace_id", "start_time"],
            Config::default(),
        )
        .unwrap();
        w.write(&batch(20_000)).unwrap();
        w.finish().unwrap().0
    }

    /// The schema of [`user_file`]: a 16-byte trace id and a nullable `user_id`.
    fn user_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("user_id", DataType::Utf8, true),
        ]))
    }

    /// A file of 5,000 rows sorted by `trace_id`. `user_id` is `user-{i % 500}`, and null
    /// when `i % 3 == 0`.
    fn user_file() -> Vec<u8> {
        let ids: Vec<u8> = (0..5_000usize)
            .flat_map(|i| ((i / 5) as u128).to_be_bytes())
            .collect();
        let ids = FixedSizeBinaryArray::try_new(16, Buffer::from_vec(ids), None).unwrap();
        let users = StringArray::from_iter(
            (0..5_000usize).map(|i| (i % 3 != 0).then(|| format!("user-{}", i % 500))),
        );
        let rows =
            RecordBatch::try_new(user_schema(), vec![Arc::new(ids), Arc::new(users)]).unwrap();
        let mut w = StrataWriter::new(user_schema(), &["trace_id"], Config::default()).unwrap();
        w.write(&rows).unwrap();
        w.finish().unwrap().0
    }

    #[test]
    fn lookup_returns_the_rows_of_present_ids() {
        runtime().block_on(async {
            let file = open_patched(batch_file()).await.unwrap();
            let decoded = crate::reader::read_column(&file, 0).await.unwrap();
            let ids = decoded.as_fixed_size_binary();
            for k in (0..4_000u128).step_by(97) {
                let key = k.to_be_bytes();
                let expected: Vec<u32> = (0..ids.len())
                    .filter(|&row| ids.value(row) == key.as_slice())
                    .map(|row| row as u32)
                    .collect();
                assert!(!expected.is_empty());
                let got: Vec<u32> = file
                    .lookup(&key)
                    .await
                    .unwrap()
                    .into_iter()
                    .flatten()
                    .collect();
                assert_eq!(got, expected, "rows of id {k}");
            }
        });
    }

    #[test]
    fn absent_ids_are_empty_and_mostly_filtered() {
        runtime().block_on(async {
            let file = open_patched(batch_file()).await.unwrap();
            let mut filtered = 0;
            for i in 0..1_000u64 {
                let key = xxh3_128(&(1_000_000 + i).to_le_bytes()).to_le_bytes();
                assert!(file.lookup(&key).await.unwrap().is_empty());
                if file.may_contain("trace_id", &key).await.unwrap() == Some(false) {
                    filtered += 1;
                }
            }
            assert!(
                filtered >= 980,
                "{filtered} of 1000 absent ids were filtered"
            );
        });
    }

    #[test]
    fn present_lookup_costs_at_most_two_gets() {
        runtime().block_on(async {
            let counting = Arc::new(Counting::new());
            let store: Arc<dyn ObjectStore> = counting.clone();
            put(store.as_ref(), batch_file()).await;
            let file = StrataFile::open(store, path(), None, &Config::default())
                .await
                .unwrap();
            let before = counting.gets();
            let rows = file.lookup(&1_234u128.to_be_bytes()).await.unwrap();
            assert!(!rows.is_empty());
            assert!(counting.gets() - before <= 2);
        });
    }

    #[test]
    fn lookup_on_an_unsupported_sort_key_is_unsupported() {
        runtime().block_on(async {
            let file = open_patched(batch_file()).await.unwrap();
            let short = file.lookup(&[0u8; 15]).await;
            assert!(matches!(short, Err(StrataError::Unsupported { .. })));

            let mut w =
                StrataWriter::new(batch_schema(), &["start_time"], Config::default()).unwrap();
            w.write(&batch(100)).unwrap();
            let file = open_patched(w.finish().unwrap().0).await.unwrap();
            let by_time = file.lookup(&[0u8; 16]).await;
            assert!(matches!(by_time, Err(StrataError::Unsupported { .. })));
        });
    }

    #[test]
    fn user_id_key_filter_answers_membership() {
        runtime().block_on(async {
            let file = open_patched(user_file()).await.unwrap();
            for j in 0..500 {
                let value = format!("user-{j}");
                let found = file.may_contain("user_id", value.as_bytes()).await;
                assert_eq!(found.unwrap(), Some(true), "{value}");
            }
            let mut absent = 0;
            for i in 0..1_000 {
                let value = format!("other-{i}");
                if file.may_contain("user_id", value.as_bytes()).await.unwrap() == Some(false) {
                    absent += 1;
                }
            }
            assert!(
                absent >= 980,
                "{absent} of 1000 absent values were filtered"
            );
        });
    }

    #[test]
    fn a_column_without_a_filter_and_an_unknown_name() {
        runtime().block_on(async {
            let file = open_patched(batch_file()).await.unwrap();
            assert_eq!(file.may_contain("input_tokens", b"1").await.unwrap(), None);
            assert!(matches!(
                file.may_contain("no_such_column", b"1").await,
                Err(StrataError::Schema { .. })
            ));
        });
    }

    #[test]
    fn zero_row_file_has_no_rows_to_find() {
        runtime().block_on(async {
            let file = open_patched(write_empty_file(batch_schema()))
                .await
                .unwrap();
            assert!(file.lookup(&[0u8; 16]).await.unwrap().is_empty());
            let found = file.may_contain("trace_id", &[0u8; 16]).await;
            assert_eq!(found.unwrap(), Some(false));
        });
    }

    #[test]
    fn flipped_key_filter_segment_is_corrupt_on_first_use() {
        runtime().block_on(async {
            let mut bytes = user_file();
            let file = open_patched(bytes.clone()).await.unwrap();
            let segment = file.hot.key_filters[0].1;
            let at = segment.offset as usize + SEGMENT_HEADER_LEN;
            bytes[at] ^= 0xFF;
            let file = open_patched(bytes).await.unwrap();
            assert!(is_corrupt(&file.may_contain("user_id", b"user-1").await));
        });
    }

    #[test]
    fn key_filter_with_bad_geometry_is_corrupt_at_open() {
        runtime().block_on(async {
            let mut bytes = populated().0;
            let hot = hot_offset_of(&bytes) as usize;
            let body = section_bounds(&bytes[hot..bytes.len() - 64])[3].clone();
            // Section 4 is the sort-key filter: tag, seed, then segment_length.
            let at = hot + body.start + 1 + 8;
            bytes[at..at + 4].copy_from_slice(&3u32.to_le_bytes());
            refresh_hot_checksum(&mut bytes);
            assert!(matches!(
                open_patched(bytes).await,
                Err(StrataError::Corrupt {
                    what: "key filter",
                    ..
                })
            ));
        });
    }

    #[test]
    fn no_sort_key_with_a_block_table_is_corrupt_at_open() {
        runtime().block_on(async {
            let mut bytes = populated().0;
            let ps = bytes.len() - 64;
            bytes[ps + 32..ps + 36].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(matches!(
                open_patched(bytes).await,
                Err(StrataError::Corrupt {
                    what: "sort-key block table",
                    ..
                })
            ));
        });
    }

    #[test]
    fn sort_key_column_out_of_the_schema_is_corrupt_at_open() {
        runtime().block_on(async {
            let mut bytes = populated().0;
            let fields = batch_schema().fields().len() as u32;
            let ps = bytes.len() - 64;
            bytes[ps + 32..ps + 36].copy_from_slice(&fields.to_le_bytes());
            assert!(matches!(
                open_patched(bytes).await,
                Err(StrataError::Corrupt {
                    what: "sort key column",
                    ..
                })
            ));
        });
    }
}
