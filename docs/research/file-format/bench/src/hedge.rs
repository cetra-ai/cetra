//! An object store that sends a second copy of a slow GET and keeps whichever copy finishes first
//! (`FMT_HEDGE_MS`, `real-data.md` § Real S3). A wave of GETs waits for its slowest request, and
//! on R2 the slowest of twenty takes about twice the median, so a copy sent after the usual time
//! of a request often finishes before the original.
//!
//! Only ranged and whole-object GETs are hedged; HEAD, LIST and writes go straight through. Each
//! copy's body is read in full before the race ends, so the winner is the first complete answer.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::{Either, select};
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// Second copies sent since the process started.
pub static HEDGED: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct Hedge {
    inner: Arc<dyn ObjectStore>,
    delay: Duration,
}

impl Hedge {
    pub fn new(inner: Arc<dyn ObjectStore>, delay: Duration) -> Self {
        Self { inner, delay }
    }
}

impl fmt::Display for Hedge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hedge({}, {} ms)", self.inner, self.delay.as_millis())
    }
}

/// One complete GET: its metadata and its whole body.
type Answer = (ObjectMeta, Range<u64>, Attributes, Bytes);

async fn fetch(inner: Arc<dyn ObjectStore>, path: Path, o: GetOptions) -> Result<Answer> {
    let r = inner.get_opts(&path, o).await?;
    let (meta, range, attributes) = (r.meta.clone(), r.range.clone(), r.attributes.clone());
    Ok((meta, range, attributes, r.bytes().await?))
}

#[async_trait]
impl ObjectStore for Hedge {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(&self, l: &Path, o: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
        if o.head {
            return self.inner.get_opts(l, o).await;
        }
        let first = Box::pin(fetch(self.inner.clone(), l.clone(), o.clone()));
        let answer = match select(first, Box::pin(tokio::time::sleep(self.delay))).await {
            Either::Left((done, _)) => done?,
            Either::Right(((), first)) => {
                HEDGED.fetch_add(1, Ordering::Relaxed);
                let second = Box::pin(fetch(self.inner.clone(), l.clone(), o));
                match select(first, second).await {
                    Either::Left((done, _)) | Either::Right((done, _)) => done?,
                }
            }
        };
        let (meta, range, attributes, body) = answer;
        let stream = futures::stream::once(async move { Ok(body) });
        Ok(GetResult { payload: GetResultPayload::Stream(Box::pin(stream)), meta, range, attributes })
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
