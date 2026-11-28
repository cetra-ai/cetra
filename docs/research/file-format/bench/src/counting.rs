//! LocalFileSystem wrapper counting read requests and bytes, to compare I/O
//! shape (what an S3 backend would pay per query).
//!
//! With `FMT_TIMELINE=1` it also records when each GET, ranged batch and LIST started, got its
//! first byte and ended, relative to the last `reset`. A GET's body is then read before the call
//! returns, so its end is known; readers that stream a body see it buffered.

use std::fmt;
use std::sync::Arc;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::path::Path;
use futures::StreamExt;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// One request of the timeline: kind, object name, bytes, and start, first byte and end in ms.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: &'static str,
    pub name: String,
    pub bytes: u64,
    pub start: f64,
    pub first: f64,
    pub end: f64,
}

#[derive(Debug)]
pub struct Counting {
    inner: Arc<dyn ObjectStore>,
    reqs: AtomicU64,
    bytes: AtomicU64,
    /// `Some` with `FMT_TIMELINE=1`: the time of the last reset and the events since.
    timeline: Option<Arc<Mutex<(Instant, Vec<Event>)>>>,
}

impl Counting {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        let timeline = std::env::var_os("FMT_TIMELINE").map(|_| Arc::new(Mutex::new((Instant::now(), Vec::new()))));
        Self { inner, reqs: AtomicU64::new(0), bytes: AtomicU64::new(0), timeline }
    }
    pub fn reset(&self) {
        self.reqs.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        if let Some(t) = &self.timeline {
            // A poisoned lock only means a request panicked; the events are still valid.
            *t.lock().unwrap_or_else(|e| e.into_inner()) = (Instant::now(), Vec::new());
        }
    }
    /// The events since the last reset, by start time (empty without `FMT_TIMELINE`).
    pub fn timeline(&self) -> Vec<Event> {
        let mut v = self.timeline.as_ref().map(|t| t.lock().unwrap_or_else(|e| e.into_inner()).1.clone()).unwrap_or_default();
        v.sort_by(|a, b| a.start.total_cmp(&b.start));
        v
    }
    fn record(timeline: &Mutex<(Instant, Vec<Event>)>, kind: &'static str, path: &Path, bytes: u64, start: Instant, first: Instant) {
        let mut t = timeline.lock().unwrap_or_else(|e| e.into_inner());
        let ms = |i: Instant| i.saturating_duration_since(t.0).as_secs_f64() * 1e3;
        let name = path.filename().unwrap_or("").to_owned();
        let e = Event { kind, name, bytes, start: ms(start), first: ms(first), end: ms(Instant::now()) };
        t.1.push(e);
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
        let start = Instant::now();
        let r = self.inner.get_opts(l, o).await?;
        let first = Instant::now();
        let len = r.range.end - r.range.start;
        if !head {
            self.reqs.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(len, Ordering::Relaxed);
        }
        let Some(t) = &self.timeline else { return Ok(r) };
        if head {
            Self::record(t, "HEAD", l, 0, start, first);
            return Ok(r);
        }
        let (meta, range, attributes) = (r.meta.clone(), r.range.clone(), r.attributes.clone());
        let body = r.bytes().await?;
        Self::record(t, "GET", l, len, start, first);
        let stream = futures::stream::once(async move { Ok(body) });
        Ok(GetResult { payload: GetResultPayload::Stream(Box::pin(stream)), meta, range, attributes })
    }
    async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let n: u64 = ranges.iter().map(|r| r.end - r.start).sum();
        self.reqs.fetch_add(ranges.len() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(n, Ordering::Relaxed);
        let start = Instant::now();
        let r = self.inner.get_ranges(l, ranges).await?;
        if let Some(t) = &self.timeline {
            Self::record(t, if ranges.len() == 1 { "GET" } else { "RANGES" }, l, n, start, start);
        }
        Ok(r)
    }
    fn delete_stream(&self, s: BoxStream<'static, Result<Path>>) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(s)
    }
    fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let Some(t) = self.timeline.clone() else { return self.inner.list(p) };
        let (start, path) = (Instant::now(), p.cloned().unwrap_or_default());
        let done = futures::stream::once(async move { Self::record(&t, "LIST", &path, 0, start, start) }).filter_map(|()| async { None });
        Box::pin(self.inner.list(p).chain(done))
    }
    async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(p).await
    }
    async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
        self.inner.copy_opts(f, t, o).await
    }
}
