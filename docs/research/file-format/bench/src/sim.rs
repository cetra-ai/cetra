//! An object store that makes local reads take as long as S3 reads would, to estimate S3 times
//! without S3 (`FMT_SIM_S3=1`, `real-data.md` § Simulated S3).
//!
//! Each GET (and HEAD) reads the local file, then waits until the time the same request would
//! end on S3: the first-byte latency, plus its bytes at one connection's throughput, and no
//! earlier than the machine's network link allows for all the bytes in flight. A LIST waits one
//! first-byte latency before its first entry. The first-byte latency is lognormal with the
//! configured median, so a wave of concurrent requests waits for its slowest one as on S3; with
//! `FMT_SIM_LATENCY_SIGMA=0` it is constant. Dependent waves, request concurrency and CPU work
//! between requests come from the reader itself, so every format pays for its own access
//! pattern. Throttling, queueing at high concurrency and retries are not modelled.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// The S3 model's constants, from `FMT_SIM_*` variables.
#[derive(Debug, Clone, Copy)]
pub struct Model {
    /// First-byte latency of a GET (`FMT_SIM_LATENCY_MS`, default 30 ms: S3 Standard in-region).
    pub latency: Duration,
    /// Throughput of one connection (`FMT_SIM_CONN_MBPS`, default 90 MB/s, AWS's guidance).
    pub conn_bytes_per_sec: f64,
    /// Throughput of the machine's link (`FMT_SIM_LINK_GBPS`, default 10 Gbit/s).
    pub link_bytes_per_sec: f64,
    /// Shape of the lognormal first-byte latency, whose median is `latency`
    /// (`FMT_SIM_LATENCY_SIGMA`, default 0: constant). R2 measured 0.35 (`real-data.md` § Real S3).
    pub sigma: f64,
}

impl Model {
    pub fn from_env() -> Self {
        let num = |name: &str, default: f64| {
            std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default)
        };
        Self {
            latency: Duration::from_secs_f64(num("FMT_SIM_LATENCY_MS", 30.0) / 1e3),
            conn_bytes_per_sec: num("FMT_SIM_CONN_MBPS", 90.0) * 1e6,
            link_bytes_per_sec: num("FMT_SIM_LINK_GBPS", 10.0) * 1e9 / 8.0,
            sigma: num("FMT_SIM_LATENCY_SIGMA", 0.0).max(0.0),
        }
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "latency {} ms (sigma {}), {} MB/s per connection, {} Gbit/s link",
            self.latency.as_secs_f64() * 1e3,
            self.sigma,
            self.conn_bytes_per_sec / 1e6,
            self.link_bytes_per_sec * 8.0 / 1e9
        )
    }
}

#[derive(Debug)]
pub struct SimS3 {
    inner: Arc<dyn ObjectStore>,
    model: Model,
    /// When the link is free again: requests share it in arrival order.
    link_free: Mutex<Instant>,
    /// xorshift64 state for the latency draws (fixed seed: runs repeat).
    rng: Mutex<u64>,
}

impl SimS3 {
    pub fn new(inner: Arc<dyn ObjectStore>, model: Model) -> Self {
        Self { inner, model, link_free: Mutex::new(Instant::now()), rng: Mutex::new(0x9e37_79b9_7f4a_7c15) }
    }

    /// A uniform draw in (0, 1).
    fn uniform(&self) -> f64 {
        // A poisoned lock only means another request panicked; the state is still valid.
        let mut x = self.rng.lock().unwrap_or_else(|e| e.into_inner());
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        ((*x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// One first-byte latency: `latency * exp(sigma * z)`, z standard normal (Box-Muller).
    fn latency(&self) -> Duration {
        if self.model.sigma == 0.0 {
            return self.model.latency;
        }
        let z = (-2.0 * self.uniform().ln()).sqrt() * (2.0 * std::f64::consts::PI * self.uniform()).cos();
        self.model.latency.mul_f64((self.model.sigma * z).exp())
    }

    /// Waits until a request that started at `start` and carries `bytes` would end on S3.
    async fn wait(&self, start: Instant, bytes: u64) {
        let bytes = bytes as f64;
        let latency = self.latency();
        let on_conn = start + latency + Duration::from_secs_f64(bytes / self.model.conn_bytes_per_sec);
        let on_link = {
            // A poisoned lock only means another request panicked; the time is still valid.
            let mut free = self.link_free.lock().unwrap_or_else(|e| e.into_inner());
            let begin = (*free).max(start + latency);
            *free = begin + Duration::from_secs_f64(bytes / self.model.link_bytes_per_sec);
            *free
        };
        tokio::time::sleep_until(on_conn.max(on_link).into()).await;
    }
}

impl fmt::Display for SimS3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimS3({}, {})", self.inner, self.model)
    }
}

#[async_trait]
impl ObjectStore for SimS3 {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(&self, l: &Path, o: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
        let start = Instant::now();
        let head = o.head;
        let r = self.inner.get_opts(l, o).await?;
        if head {
            self.wait(start, 0).await;
            return Ok(r);
        }
        // Read the bytes now, so that the wait covers the whole transfer as on S3.
        let (meta, range, attributes) = (r.meta.clone(), r.range.clone(), r.attributes.clone());
        let bytes = r.bytes().await?;
        self.wait(start, bytes.len() as u64).await;
        let stream = futures::stream::once(async move { Ok(bytes) });
        Ok(GetResult { payload: GetResultPayload::Stream(Box::pin(stream)), meta, range, attributes })
    }
    async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        // As the default implementation: one GET per range, all at once.
        futures::future::try_join_all(ranges.iter().map(|r| self.get_range(l, r.clone()))).await
    }
    fn delete_stream(&self, s: BoxStream<'static, Result<Path>>) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(s)
    }
    fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        use futures::StreamExt;
        let latency = self.latency();
        let wait = futures::stream::once(tokio::time::sleep(latency)).filter_map(|()| async { None });
        Box::pin(wait.chain(self.inner.list(p)))
    }
    async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
        let start = Instant::now();
        let r = self.inner.list_with_delimiter(p).await?;
        tokio::time::sleep_until((start + self.latency()).into()).await;
        Ok(r)
    }
    async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
        self.inner.copy_opts(f, t, o).await
    }
}
