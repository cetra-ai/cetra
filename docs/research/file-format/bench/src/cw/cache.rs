//! The read cache of cetra's S3 reads (spec 03 § 6) as an in-memory LRU of 1 MiB blocks in front of
//! an object store (plans/08-m5-memory-concurrency.md § M5.2).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// Block size of cetra's S3 read cache (spec 03 § 6).
pub const BLOCK: u64 = 1 << 20;

/// The held blocks with their last-use stamp, and the stamps in order for eviction.
#[derive(Debug, Default)]
struct Lru {
    blocks: HashMap<(Path, u64), (Bytes, u64)>,
    order: BTreeMap<u64, (Path, u64)>,
    bytes: u64,
    stamp: u64,
}

impl Lru {
    /// The block `key` if it is held; a held block becomes the most recently used.
    fn touch(&mut self, key: &(Path, u64)) -> Option<Bytes> {
        let (bytes, used) = self.blocks.get_mut(key)?;
        self.order.remove(&*used);
        self.stamp += 1;
        *used = self.stamp;
        self.order.insert(self.stamp, key.clone());
        Some(bytes.clone())
    }

    /// Holds `bytes` as block `key`, then evicts the least recently used blocks while more than
    /// `budget` bytes are held.
    fn insert(&mut self, key: (Path, u64), bytes: Bytes, budget: u64) {
        if let Some((old, used)) = self.blocks.remove(&key) {
            self.order.remove(&used);
            self.bytes -= old.len() as u64;
        }
        self.stamp += 1;
        self.bytes += bytes.len() as u64;
        self.order.insert(self.stamp, key.clone());
        self.blocks.insert(key, (bytes, self.stamp));
        while self.bytes > budget {
            let Some((_, victim)) = self.order.pop_first() else {
                break;
            };
            if let Some((old, _)) = self.blocks.remove(&victim) {
                self.bytes -= old.len() as u64;
            }
        }
    }
}

/// An in-memory LRU of `BLOCK`-aligned blocks in front of `inner`, as cetra's read cache behaves
/// (spec 03 § 6): a read serves the blocks it holds and fetches the missing ones in one coalesced
/// request (from the first missing block's start to the last missing block's end; the held blocks
/// inside that span are fetched again and count as misses), then keeps every fetched block,
/// evicting the least recently used blocks beyond `budget` bytes. LIST and writes pass through.
/// The object's last block is short.
#[derive(Debug)]
pub struct BlockCache {
    inner: Arc<dyn ObjectStore>,
    budget: u64,
    lru: Mutex<Lru>,
    /// The metadata of every object read or headed, so a read does not HEAD the object again.
    metas: Mutex<HashMap<Path, ObjectMeta>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Locks `m`. A poisoned lock only means a holder panicked; the data is still valid.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The error of a read the cache cannot complete.
fn cache_error(message: &str) -> object_store::Error {
    object_store::Error::Generic {
        store: "BlockCache",
        source: message.into(),
    }
}

/// The byte range a read of `range` (or the whole object when `None`) resolves to in an object of
/// `size` bytes. `None` when it is empty or starts at or past the end: such reads are passed to
/// the inner store, which answers them as it does.
fn resolve(range: Option<&GetRange>, size: u64) -> Option<Range<u64>> {
    let r = match range {
        None => 0..size,
        Some(GetRange::Bounded(r)) => r.start..r.end.min(size),
        Some(GetRange::Offset(a)) => *a..size,
        Some(GetRange::Suffix(n)) => size - (*n).min(size)..size,
    };
    (r.start < r.end && r.start < size).then_some(r)
}

impl BlockCache {
    pub fn new(inner: Arc<dyn ObjectStore>, budget: u64) -> Self {
        Self {
            inner,
            budget,
            lru: Mutex::new(Lru::default()),
            metas: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// (hits, misses) in blocks since the last reset.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Zeroes the counters only; the cached blocks and remembered sizes stay.
    pub fn reset_stats(&self) {
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// The object's metadata: remembered, or read with one HEAD on the inner store.
    async fn meta(&self, l: &Path) -> Result<ObjectMeta> {
        let known = lock(&self.metas).get(l).cloned();
        if let Some(m) = known {
            return Ok(m);
        }
        let m = self
            .inner
            .get_opts(
                l,
                GetOptions {
                    head: true,
                    ..Default::default()
                },
            )
            .await?
            .meta;
        lock(&self.metas).insert(l.clone(), m.clone());
        Ok(m)
    }

    /// The bytes of `range`, a valid range of an object of `size` bytes: the held blocks are
    /// served, the missing ones are fetched in one request and kept.
    async fn read(&self, l: &Path, size: u64, range: Range<u64>) -> Result<Bytes> {
        let (first, last) = (range.start / BLOCK, (range.end - 1) / BLOCK);
        // Each block of the range with its bytes if it is held (touched as the most recent).
        let found: Vec<(u64, Option<Bytes>)> = {
            let mut lru = lock(&self.lru);
            (first..=last)
                .map(|k| (k, lru.touch(&(l.clone(), k))))
                .collect()
        };
        let missing: Vec<u64> = found
            .iter()
            .filter(|(_, b)| b.is_none())
            .map(|(k, _)| *k)
            .collect();
        let span = match (missing.first(), missing.last()) {
            (Some(&a), Some(&b)) => Some((a, b)),
            _ => None,
        };
        let in_span = |k: u64| span.is_some_and(|(a, b)| (a..=b).contains(&k));
        // The fetched blocks as their own copies, in order.
        let mut fetched: Vec<(u64, Bytes)> = Vec::new();
        if let Some((a, b)) = span {
            let (start, end) = (a * BLOCK, ((b + 1) * BLOCK).min(size));
            let data = self
                .inner
                .get_opts(
                    l,
                    GetOptions {
                        range: Some(GetRange::Bounded(start..end)),
                        ..Default::default()
                    },
                )
                .await?
                .bytes()
                .await?;
            if data.len() as u64 != end - start {
                return Err(cache_error("the inner store returned a short range"));
            }
            for k in a..=b {
                let s = (k * BLOCK - start) as usize;
                let e = (((k + 1) * BLOCK).min(size) - start) as usize;
                fetched.push((k, Bytes::copy_from_slice(&data[s..e])));
            }
        }
        // The answer is assembled from the held and fetched blocks, not from the LRU, so a read
        // longer than the budget keeps all of its blocks.
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        let (mut hits, misses) = (0u64, fetched.len() as u64);
        for (k, held) in found {
            let block = match (in_span(k), held) {
                (false, Some(b)) => {
                    hits += 1;
                    b
                }
                (true, _) => fetched
                    .iter()
                    .find(|(f, _)| *f == k)
                    .map(|(_, b)| b.clone())
                    .ok_or_else(|| cache_error("a fetched block is missing"))?,
                (false, None) => return Err(cache_error("a held block is missing")),
            };
            let bs = k * BLOCK;
            let lo = range.start.max(bs) - bs;
            let hi = range.end.min(bs + block.len() as u64) - bs;
            let part = block
                .get(lo as usize..hi as usize)
                .ok_or_else(|| cache_error("a block is shorter than its range"))?;
            out.extend_from_slice(part);
        }
        if !fetched.is_empty() {
            let mut lru = lock(&self.lru);
            for (k, copy) in fetched {
                lru.insert((l.clone(), k), copy, self.budget);
            }
        }
        self.hits.fetch_add(hits, Ordering::Relaxed);
        self.misses.fetch_add(misses, Ordering::Relaxed);
        Ok(Bytes::from(out))
    }
}

impl fmt::Display for BlockCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlockCache({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for BlockCache {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &Path,
        o: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
        if o.head {
            let r = self.inner.get_opts(l, o).await?;
            lock(&self.metas).insert(l.clone(), r.meta.clone());
            return Ok(r);
        }
        let conditional = o.if_match.is_some()
            || o.if_none_match.is_some()
            || o.if_modified_since.is_some()
            || o.if_unmodified_since.is_some()
            || o.version.is_some();
        if conditional {
            return self.inner.get_opts(l, o).await;
        }
        let meta = self.meta(l).await?;
        let Some(range) = resolve(o.range.as_ref(), meta.size) else {
            return self.inner.get_opts(l, o).await;
        };
        let bytes = self.read(l, meta.size, range.clone()).await?;
        let stream = futures::stream::once(async move { Ok(bytes) });
        Ok(GetResult {
            payload: GetResultPayload::Stream(Box::pin(stream)),
            meta,
            range,
            attributes: Default::default(),
        })
    }
    async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let size = self.meta(l).await?.size;
        // The ranges are read 10 at a time, as object_store's own `get_ranges` runs the ranges of
        // one call (`buffered` keeps input order). The futures are built into a Vec first: a
        // `.map` closure in the stream's type is not general enough for a spawned caller.
        let reads: Vec<_> = ranges
            .iter()
            .map(|r| async move {
                match resolve(Some(&GetRange::Bounded(r.clone())), size) {
                    Some(valid) => self.read(l, size, valid).await.map(|b| vec![b]),
                    None => self.inner.get_ranges(l, std::slice::from_ref(r)).await,
                }
            })
            .collect();
        let parts: Vec<Vec<Bytes>> = futures::stream::iter(reads)
            .buffered(10)
            .try_collect()
            .await?;
        Ok(parts.into_iter().flatten().collect())
    }
    fn delete_stream(
        &self,
        s: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(s)
    }
    fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(p)
    }
    async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
        self.inner.copy_opts(f, t, o).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counting::Counting;
    use object_store::PutOptions;
    use object_store::memory::InMemory;
    use std::sync::atomic::AtomicU64;

    const MIB: u64 = 1 << 20;

    /// A test store over `InMemory` that counts the HEAD requests and the other GETs it receives,
    /// and yields once before each GET, so that concurrent reads interleave. Its other methods
    /// pass through, as `Counting`'s do.
    #[derive(Debug)]
    struct Probe {
        inner: Arc<dyn ObjectStore>,
        heads: AtomicU64,
        gets: AtomicU64,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                inner: Arc::new(InMemory::new()),
                heads: AtomicU64::new(0),
                gets: AtomicU64::new(0),
            }
        }
        /// (HEAD requests, other GETs) so far.
        fn counts(&self) -> (u64, u64) {
            (
                self.heads.load(Ordering::Relaxed),
                self.gets.load(Ordering::Relaxed),
            )
        }
    }

    impl fmt::Display for Probe {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Probe({})", self.inner)
        }
    }

    #[async_trait]
    impl ObjectStore for Probe {
        async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
            self.inner.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(
            &self,
            l: &Path,
            o: PutMultipartOptions,
        ) -> Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
            if o.head {
                self.heads.fetch_add(1, Ordering::Relaxed);
            } else {
                self.gets.fetch_add(1, Ordering::Relaxed);
            }
            tokio::task::yield_now().await;
            self.inner.get_opts(l, o).await
        }
        async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
            self.inner.get_ranges(l, ranges).await
        }
        fn delete_stream(
            &self,
            s: BoxStream<'static, Result<Path>>,
        ) -> BoxStream<'static, Result<Path>> {
            self.inner.delete_stream(s)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
            self.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
            self.inner.copy_opts(f, t, o).await
        }
    }

    /// The bytes `i % 251` for `i` in `0..size`, as one object `obj` of a counting store. Its
    /// counters are zeroed after the write.
    async fn stored(size: u64) -> (Arc<Counting>, Path, Vec<u8>) {
        let counting = Arc::new(Counting::new(Arc::new(InMemory::new())));
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let path = Path::from("obj");
        counting
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::from(data.clone())),
                PutOptions::default(),
            )
            .await
            .unwrap();
        counting.reset();
        (counting, path, data)
    }

    fn opts(range: Option<GetRange>) -> GetOptions {
        GetOptions {
            range,
            ..Default::default()
        }
    }

    /// The bytes of a read of `range` through `store`.
    async fn get(store: &dyn ObjectStore, path: &Path, range: Option<GetRange>) -> Bytes {
        store
            .get_opts(path, opts(range))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn block_cache_serves_what_it_holds() {
        let (inner, path, data) = stored(5 * MIB).await;
        let cache = BlockCache::new(inner.clone(), 64 * MIB);
        let got = get(&cache, &path, Some(GetRange::Bounded(0..MIB + MIB / 2))).await;
        assert_eq!(&got[..], &data[..(MIB + MIB / 2) as usize]);
        assert_eq!(inner.stats().0, 1, "blocks 0-1 are one request");
        assert_eq!(inner.stats().1, 2 * MIB);

        inner.reset();
        let got = get(&cache, &path, Some(GetRange::Bounded(MIB / 2..MIB))).await;
        assert_eq!(&got[..], &data[(MIB / 2) as usize..MIB as usize]);
        assert_eq!(inner.stats().0, 0, "block 0 is a hit");
        assert_eq!(
            cache.stats(),
            (1, 2),
            "one hit, then the two misses of the first read"
        );

        inner.reset();
        cache.reset_stats();
        let got = get(
            &cache,
            &path,
            Some(GetRange::Bounded(3 * MIB / 2..7 * MIB / 2)),
        )
        .await;
        assert_eq!(
            &got[..],
            &data[(3 * MIB / 2) as usize..(7 * MIB / 2) as usize]
        );
        assert_eq!(inner.stats().0, 1, "blocks 2-3 are one request");
        assert_eq!(inner.stats().1, 2 * MIB, "from 2 MiB to 4 MiB");
    }

    #[tokio::test]
    async fn block_cache_evicts_least_recent() {
        let (inner, path, _) = stored(5 * MIB).await;
        let cache = BlockCache::new(inner.clone(), 2 * MIB);
        for k in [0, 1, 0, 2] {
            get(&cache, &path, Some(GetRange::Bounded(k * MIB..k * MIB + 1))).await;
        }
        inner.reset();
        get(&cache, &path, Some(GetRange::Bounded(0..1))).await;
        assert_eq!(inner.stats().0, 0, "block 0 stays");
        inner.reset();
        get(&cache, &path, Some(GetRange::Bounded(MIB..MIB + 1))).await;
        assert_eq!(inner.stats().0, 1, "block 1 was evicted");
    }

    #[tokio::test]
    async fn block_cache_answers_like_the_store() {
        let size = 10 * MIB + 1234;
        let (inner, path, _) = stored(size).await;
        let cache = BlockCache::new(inner.clone(), 4 * MIB);
        let mut x: u64 = 1;
        let mut next = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            x
        };
        for _ in 0..200 {
            let kind = next() % 4;
            let range = match kind {
                0 => {
                    let a = next() % size;
                    let len = 1 + next() % (size - a);
                    Some(GetRange::Bounded(a..a + len))
                }
                1 => Some(GetRange::Offset(next() % size)),
                2 => Some(GetRange::Suffix(1 + next() % size)),
                _ => None,
            };
            let mine = cache.get_opts(&path, opts(range.clone())).await.unwrap();
            let theirs = inner.get_opts(&path, opts(range)).await.unwrap();
            assert_eq!(mine.range, theirs.range);
            assert_eq!(mine.meta.size, theirs.meta.size);
            assert_eq!(mine.bytes().await.unwrap(), theirs.bytes().await.unwrap());
        }
    }

    #[tokio::test]
    async fn block_cache_range_kinds() {
        let size = 10 * MIB + 1234;
        let (inner, path, data) = stored(size).await;
        let cache = BlockCache::new(inner.clone(), 64 * MIB);
        // A suffix longer than the object is the whole object.
        let whole = get(&cache, &path, Some(GetRange::Suffix(2 * size))).await;
        assert_eq!(&whole[..], &data[..]);
        // A bounded range past the end is clamped.
        let r = cache
            .get_opts(&path, opts(Some(GetRange::Bounded(size - 10..size + 100))))
            .await
            .unwrap();
        assert_eq!(r.range, size - 10..size);
        assert_eq!(r.bytes().await.unwrap().len(), 10);
        // The short last block returns its real length.
        let last = get(&cache, &path, Some(GetRange::Bounded(10 * MIB..size))).await;
        assert_eq!(&last[..], &data[(10 * MIB) as usize..]);
        assert_eq!(last.len(), 1234);
        // Empty and past-the-end ranges fail as the inner store does.
        for range in [GetRange::Offset(size), GetRange::Bounded(5..5)] {
            assert!(
                inner
                    .get_opts(&path, opts(Some(range.clone())))
                    .await
                    .is_err()
            );
            assert!(cache.get_opts(&path, opts(Some(range))).await.is_err());
        }
    }

    #[tokio::test]
    async fn block_cache_get_ranges() {
        let (inner, path, data) = stored(5 * MIB).await;
        let cache = BlockCache::new(inner.clone(), 64 * MIB);
        // Blocks 0 and 2 are held; block 3 is not.
        get(&cache, &path, Some(GetRange::Bounded(0..1))).await;
        get(&cache, &path, Some(GetRange::Bounded(2 * MIB..2 * MIB + 1))).await;
        inner.reset();
        let ranges = [0..10, 2 * MIB + 5..2 * MIB + 10, 3 * MIB..3 * MIB + 7];
        let got = cache.get_ranges(&path, &ranges).await.unwrap();
        assert_eq!(inner.stats().0, 1, "one request for the missing block");
        for (bytes, r) in got.iter().zip(&ranges) {
            assert_eq!(&bytes[..], &data[r.start as usize..r.end as usize]);
        }
    }

    #[tokio::test]
    async fn block_cache_get_ranges_keeps_order() {
        let (inner, path, data) = stored(5 * MIB).await;
        let cache = BlockCache::new(inner.clone(), 64 * MIB);
        // Block 1 is held; blocks 0, 2, 3 and 4 are not. Ranges 0 and 2 share block 3.
        get(&cache, &path, Some(GetRange::Bounded(MIB..MIB + 1))).await;
        let ranges = [
            3 * MIB + 100..3 * MIB + 200,
            MIB + 10..MIB + 20,
            4 * MIB - 50..4 * MIB + 50,
            100..200,
            2 * MIB + 7..2 * MIB + 9,
        ];
        let got = cache.get_ranges(&path, &ranges).await.unwrap();
        let want: Vec<Bytes> = ranges
            .iter()
            .map(|r| Bytes::copy_from_slice(&data[r.start as usize..r.end as usize]))
            .collect();
        assert_eq!(got, want);
        let inner_got = inner.get_ranges(&path, &ranges).await.unwrap();
        assert_eq!(
            got, inner_got,
            "the same bytes as the inner store, in input order"
        );
    }

    #[tokio::test]
    async fn block_cache_head_then_read() {
        let probe = Arc::new(Probe::new());
        let path = Path::from("obj");
        probe
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::from(vec![7u8; 3 * MIB as usize])),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let cache = BlockCache::new(probe.clone(), 64 * MIB);
        let head = cache
            .get_opts(
                &path,
                GetOptions {
                    head: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(head.meta.size, 3 * MIB);
        assert_eq!(probe.counts().0, 1);
        get(&cache, &path, Some(GetRange::Bounded(0..10))).await;
        get(&cache, &path, Some(GetRange::Bounded(MIB..MIB + 10))).await;
        assert_eq!(
            probe.counts().0,
            1,
            "the ranged reads use the remembered size"
        );
    }

    #[tokio::test]
    async fn block_cache_longer_than_budget() {
        let (inner, path, data) = stored(5 * MIB).await;
        let cache = BlockCache::new(inner, MIB);
        let got = get(&cache, &path, Some(GetRange::Bounded(0..3 * MIB))).await;
        assert_eq!(&got[..], &data[..(3 * MIB) as usize]);
    }

    #[tokio::test]
    async fn block_cache_concurrent_misses() {
        let probe = Arc::new(Probe::new());
        let path = Path::from("obj");
        probe
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::from(vec![9u8; 5 * MIB as usize])),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let cache = BlockCache::new(probe.clone(), 64 * MIB);
        let (a, b) = tokio::join!(
            cache.get_opts(&path, opts(Some(GetRange::Bounded(0..10)))),
            cache.get_opts(&path, opts(Some(GetRange::Bounded(0..10))))
        );
        assert_eq!(a.unwrap().bytes().await.unwrap().len(), 10);
        assert_eq!(b.unwrap().bytes().await.unwrap().len(), 10);
        assert_eq!(
            probe.counts().1,
            2,
            "no single flight: both reads fetch block 0"
        );
    }
}
