//! Opening a Strata file from an object store with one suffix read (S1 open).

use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{HEADER_LEN, POSTSCRIPT_LEN, checksum32};
use crate::hot::{DirectoryRecord, HotTail, Postscript};
use crate::segment::{Kind, SegmentRef, decode_segment};

#[cfg(test)]
#[path = "../tests/common/counting.rs"]
mod counting;

/// An open Strata file: its postscript and decoded hot tail, plus the store to read
/// segments from.
pub struct StrataFile {
    store: Arc<dyn ObjectStore>,
    path: Path,
    size: u64,
    postscript: Postscript,
    hot: HotTail,
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
        Ok(StrataFile {
            store,
            path,
            size,
            postscript,
            hot,
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
        let end = r
            .offset
            .checked_add(u64::from(r.len))
            .ok_or(StrataError::Corrupt {
                what: "segment range",
                offset: r.offset,
            })?;
        let bytes = self.store.get_range(&self.path, r.offset..end).await?;
        decode_segment(&bytes, r.offset, kind)
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
    use crate::hot::write_empty_file;
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
            assert!(file.directory().is_empty());
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
}
