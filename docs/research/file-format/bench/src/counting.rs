//! LocalFileSystem wrapper counting read requests and bytes, to compare I/O
//! shape (what an S3 backend would pay per query).

use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

#[derive(Debug)]
pub struct Counting {
    inner: LocalFileSystem,
    reqs: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    pub fn new() -> Self {
        Self { inner: LocalFileSystem::new(), reqs: AtomicU64::new(0), bytes: AtomicU64::new(0) }
    }
    pub fn reset(&self) {
        self.reqs.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
    pub fn stats(&self) -> (u64, u64) {
        (self.reqs.load(Ordering::Relaxed), self.bytes.load(Ordering::Relaxed))
    }
}

impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for Counting {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(&self, l: &Path, o: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
        let head = o.head;
        let r = self.inner.get_opts(l, o).await?;
        if !head {
            self.reqs.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(r.range.end - r.range.start, Ordering::Relaxed);
        }
        Ok(r)
    }
    async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        self.reqs.fetch_add(ranges.len() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(ranges.iter().map(|r| r.end - r.start).sum(), Ordering::Relaxed);
        self.inner.get_ranges(l, ranges).await
    }
    fn delete_stream(&self, s: BoxStream<'static, Result<Path>>) -> BoxStream<'static, Result<Path>> {
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
