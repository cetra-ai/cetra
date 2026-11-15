//! Opening a Strata file from an object store with one suffix read (S1 open).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use arrow::datatypes::SchemaRef;
use futures::future::try_join_all;
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{
    Cursor, HEADER_LEN, MAX_PAGE_ENTRIES, POSTSCRIPT_LEN, SEGMENT_HEADER_LEN, checksum32,
};
use crate::hot::{DirectoryRecord, HotTail, Layer, Postscript};
use crate::narrow::BlockEntry;
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

/// An open Strata file: its postscript and decoded hot tail, plus the store to read
/// segments from.
pub struct StrataFile {
    store: Arc<dyn ObjectStore>,
    path: Path,
    size: u64,
    postscript: Postscript,
    pub(crate) hot: HotTail,
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
            if i < n_fields && !matches!(record.layer, Layer::Narrow | Layer::Payload) {
                return Err(StrataError::Corrupt {
                    what: "directory layer",
                    offset: postscript.hot_offset,
                });
            }
        }
        Ok(StrataFile {
            store,
            path,
            size,
            postscript,
            hot,
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
    use crate::format::PAGE_REF_LEN;
    use crate::testgen::{assert_same, batch, schema as batch_schema};
    use crate::writer::StrataWriter;
    use counting::Counting;
    use llmobs_core::schema::spans_schema;
    use object_store::PutPayload;
    use object_store::memory::InMemory;
    use proptest::prelude::*;

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
            // Every column has a directory record, even with no chunks.
            assert_eq!(file.directory().len(), spans_schema().fields().len());
            assert!(file.directory().iter().all(|r| r.n_chunks == 0));
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

            // Record 0 has the Residual layer, which a schema column may not have in M1.
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
}
