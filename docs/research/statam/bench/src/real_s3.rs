use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use cetra_statam::{
    IoCounters, RangeReader, S3_CACHE_BLOCK_BYTES, SimulatedS3Config, SimulatedS3Reader,
    StatamError,
};
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt, TryStreamExt};
use object_store_14::aws::AmazonS3Builder;
use object_store_14::path::Path as ObjectPath14;
use object_store_14::{
    ClientOptions, ObjectStore as ObjectStore14, ObjectStoreExt as ObjectStoreExt14, PutPayload,
};
use parquet::arrow::arrow_reader::ArrowReaderOptions;
use parquet::arrow::async_reader::AsyncFileReader;
use parquet::errors::ParquetError;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader};
use sha2::{Digest, Sha256};

use crate::schema::BenchResult;

/// Coalesce gap used in `Direct` real-S3 mode for batched `read_ranges` calls
/// across Parquet (`AsyncFileReader::get_byte_ranges`), Lance (`ObjectStore::get_ranges`),
/// and Statam (`RangeReader::read_ranges`).
const DIRECT_COALESCE_GAP_BYTES: u64 = 64 * 1024;
/// Maximum concurrent in-flight HTTP range GETs per `read_ranges` batch.
const MAX_CONCURRENT_GETS: usize = 16;
/// Multipart upload threshold and part size (8 MiB).
const MULTIPART_THRESHOLD_BYTES: usize = 16 * 1024 * 1024;
const MULTIPART_PART_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealS3Mode {
    /// Existing in-memory 1 MiB ReadCache S3 simulator (default when `STATAM_REAL_S3` is unset).
    Sim,
    /// In-memory Direct S3 simulator (exact HTTP Range GETs + 64 KiB coalescing).
    SimDirect,
    /// Real remote S3 (`AmazonS3`), exact HTTP Range GETs (no 1 MiB rounding, no block cache).
    Direct,
    /// Real remote S3 (`AmazonS3`) behind a per-reader 1 MiB ReadCache matching Cetra's ReadCache.
    ReadCache1MiB,
}

impl RealS3Mode {
    pub fn from_env() -> Self {
        match std::env::var("STATAM_REAL_S3")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "sim-direct" | "sim_direct" | "simdirect" => Self::SimDirect,
            "direct" | "raw" => Self::Direct,
            "readcache" | "read_cache" | "cache" | "1mib" => Self::ReadCache1MiB,
            _ => Self::Sim,
        }
    }

    pub fn is_real(self) -> bool {
        matches!(self, Self::Direct | Self::ReadCache1MiB)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sim => "sim-readcache-1mib",
            Self::SimDirect => "sim-direct",
            Self::Direct => "real-s3-direct",
            Self::ReadCache1MiB => "real-s3-readcache-1mib",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimS3Profile {
    Default,
    WanGarage,
}

impl SimS3Profile {
    pub fn from_env() -> Self {
        match std::env::var("STATAM_S3_PROFILE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "garage" | "wan-garage" | "wan_garage" => Self::WanGarage,
            _ => Self::Default,
        }
    }

    /// Effective simulated profile for `mode` (`SimDirect` always uses `WanGarage`).
    pub fn effective(mode: RealS3Mode) -> Self {
        if mode == RealS3Mode::SimDirect {
            Self::WanGarage
        } else {
            Self::from_env()
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::WanGarage => "wan-garage",
        }
    }
}

/// Human-readable description of the active simulated S3 mode & profile for report headers.
pub fn sim_s3_header_summary(mode: RealS3Mode) -> String {
    let profile = SimS3Profile::effective(mode);
    let cfg = match (mode, profile) {
        (RealS3Mode::SimDirect, _) => {
            SimulatedS3Config::wan_garage().with_direct_mode(DIRECT_COALESCE_GAP_BYTES)
        }
        (_, SimS3Profile::WanGarage) => SimulatedS3Config::wan_garage(),
        (_, SimS3Profile::Default) => SimulatedS3Config::default(),
    };
    format!(
        "`{}` (profile=`{}`, `rtt={:.1} ms`, `agg={:.0} MiB/s`, `stream={:.0} MiB/s`, `conc={}`)",
        mode.label(),
        profile.label(),
        cfg.rtt_ms,
        cfg.bandwidth_bytes_per_sec / (1024.0 * 1024.0),
        cfg.per_stream_bandwidth_bytes_per_sec / (1024.0 * 1024.0),
        cfg.max_concurrent_gets,
    )
}

/// Returns `true` when `STATAM_S3_ONLY=1` is set (skip warm local CPU timing loops).
pub fn s3_only_mode() -> bool {
    matches!(
        std::env::var("STATAM_S3_ONLY").as_deref(),
        Ok("1" | "true" | "yes" | "TRUE")
    )
}

/// Returns `true` when `STATAM_S3_TRACE=1` is set (opt-in privacy-safe per-GET stderr trace).
fn s3_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("STATAM_S3_TRACE").as_deref(),
            Ok("1" | "true" | "yes" | "TRUE")
        )
    })
}

/// Shared real S3 client + idempotent content-addressed uploader.
pub struct RealS3Client {
    store: Arc<dyn ObjectStore14>,
    prefix: String,
    endpoint: String,
    bucket: String,
    uploaded: Mutex<HashSet<String>>,
}

static REAL_S3_CLIENT: OnceLock<Arc<RealS3Client>> = OnceLock::new();

impl RealS3Client {
    pub fn global() -> BenchResult<Arc<Self>> {
        if let Some(existing) = REAL_S3_CLIENT.get() {
            return Ok(Arc::clone(existing));
        }
        let endpoint = std::env::var("AWS_ENDPOINT_URL")
            .map_err(|_| "STATAM_REAL_S3 requires AWS_ENDPOINT_URL")?;
        let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| "STATAM_REAL_S3 requires AWS_ACCESS_KEY_ID")?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| "STATAM_REAL_S3 requires AWS_SECRET_ACCESS_KEY")?;
        let bucket = std::env::var("STATAM_S3_BUCKET")
            .or_else(|_| std::env::var("CETRA_TEST_S3_BUCKET"))
            .unwrap_or_else(|_| "statam-bench".to_owned());
        let prefix = std::env::var("STATAM_S3_PREFIX").unwrap_or_else(|_| "v1".to_owned());

        let client_opts = ClientOptions::new()
            .with_timeout(Duration::from_secs(180))
            .with_connect_timeout(Duration::from_secs(15))
            .with_pool_max_idle_per_host(32);

        let s3 = AmazonS3Builder::new()
            .with_endpoint(&endpoint)
            .with_region(&region)
            .with_bucket_name(&bucket)
            .with_access_key_id(&access_key)
            .with_secret_access_key(&secret_key)
            .with_virtual_hosted_style_request(false)
            .with_allow_http(endpoint.starts_with("http://"))
            .with_client_options(client_opts)
            .build()?;

        let client = Arc::new(Self {
            store: Arc::new(s3),
            prefix,
            endpoint,
            bucket,
            uploaded: Mutex::new(HashSet::new()),
        });
        let _ = REAL_S3_CLIENT.set(Arc::clone(&client));
        Ok(REAL_S3_CLIENT.get().map(Arc::clone).unwrap_or(client))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn content_key(&self, bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(20);
        for b in &digest[..10] {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{b:02x}");
        }
        format!("{}/{hex}-{}.bin", self.prefix, bytes.len())
    }

    /// Uploads `bytes` to S3 under its content-addressed key if not already present with matching size.
    pub async fn ensure_uploaded(&self, bytes: &Bytes) -> BenchResult<ObjectPath14> {
        let key = self.content_key(bytes);
        let path = ObjectPath14::from(key.as_str());
        {
            let guard = self.uploaded.lock().unwrap_or_else(|p| p.into_inner());
            if guard.contains(&key) {
                return Ok(path);
            }
        }

        if let Ok(meta) = self.store.head(&path).await
            && meta.size == bytes.len() as u64
        {
            let mut guard = self.uploaded.lock().unwrap_or_else(|p| p.into_inner());
            guard.insert(key);
            return Ok(path);
        }

        let t0 = Instant::now();
        if bytes.len() <= MULTIPART_THRESHOLD_BYTES {
            self.store
                .put(&path, PutPayload::from_bytes(bytes.clone()))
                .await?;
        } else {
            let mut mp = self.store.put_multipart(&path).await?;
            let mut offset = 0usize;
            while offset < bytes.len() {
                let end = (offset + MULTIPART_PART_BYTES).min(bytes.len());
                mp.put_part(PutPayload::from_bytes(bytes.slice(offset..end)))
                    .await?;
                offset = end;
            }
            mp.complete().await?;
        }
        eprintln!(
            "[real-s3] uploaded {key} ({:.1} KiB) in {:.1} ms",
            bytes.len() as f64 / 1024.0,
            t0.elapsed().as_secs_f64() * 1000.0
        );

        let mut guard = self.uploaded.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(key);
        Ok(path)
    }
}

fn run_blocking_on<F: Future>(handle: &tokio::runtime::Handle, fut: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| handle.block_on(fut))
    } else {
        handle.block_on(fut)
    }
}

type InflightResult = Option<Result<(), String>>;
type InflightTx = tokio::sync::watch::Sender<InflightResult>;
type InflightRx = tokio::sync::watch::Receiver<InflightResult>;

type SimBatchItem = (
    Range<u64>,
    tokio::sync::oneshot::Sender<Result<Bytes, StatamError>>,
);

struct SimBatchQueue {
    pending: Vec<SimBatchItem>,
    flushing: bool,
}

/// Cancel-safe async micro-batcher for simulated S3 `read_range_async` calls so concurrent
/// futures (e.g. Vortex `VortexReadAt` and Lance `ScanScheduler`) schedule into one parallel wave.
struct SimAsyncBatcher {
    queue: Mutex<SimBatchQueue>,
}

struct LeaderGuard<'a> {
    batcher: &'a SimAsyncBatcher,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let drained = {
            let mut guard = self
                .batcher
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            guard.flushing = false;
            std::mem::take(&mut guard.pending)
        };
        drop(drained);
    }
}

impl SimAsyncBatcher {
    fn new() -> Self {
        Self {
            queue: Mutex::new(SimBatchQueue {
                pending: Vec::new(),
                flushing: false,
            }),
        }
    }

    async fn read_range(
        &self,
        sim: &SimulatedS3Reader,
        range: Range<u64>,
    ) -> Result<Bytes, StatamError> {
        loop {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let is_leader = {
                let mut guard = self.queue.lock().unwrap_or_else(|p| p.into_inner());
                guard.pending.push((range.clone(), tx));
                if !guard.flushing {
                    guard.flushing = true;
                    true
                } else {
                    false
                }
            };

            if is_leader {
                let mut leader_guard = LeaderGuard {
                    batcher: self,
                    armed: true,
                };
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;

                let batch = {
                    let mut guard = self.queue.lock().unwrap_or_else(|p| p.into_inner());
                    guard.flushing = false;
                    leader_guard.armed = false;
                    std::mem::take(&mut guard.pending)
                };

                let ranges: Vec<Range<u64>> = batch.iter().map(|(r, _)| r.clone()).collect();
                match sim.read_ranges_uncoalesced(&ranges) {
                    Ok(parts) if parts.len() == batch.len() => {
                        for ((_, sender), part) in batch.into_iter().zip(parts) {
                            let _ = sender.send(Ok(part));
                        }
                    }
                    Ok(_) => {
                        for (_, sender) in batch {
                            let _ = sender.send(Err(StatamError::Corrupt {
                                reason: "sim batcher range count mismatch",
                            }));
                        }
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        let mut iter = batch.into_iter();
                        if let Some((_, first_sender)) = iter.next() {
                            let _ = first_sender.send(Err(err));
                        }
                        for (_, sender) in iter {
                            let _ = sender
                                .send(Err(StatamError::Io(std::io::Error::other(msg.clone()))));
                        }
                    }
                }
            }

            match rx.await {
                Ok(res) => return res,
                Err(_) => {
                    // Leader task was dropped while parked; retry so this surviving waiter
                    // starts or joins a fresh simulated wave.
                    continue;
                }
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum BenchS3Inner {
    Sim {
        sim: Arc<SimulatedS3Reader>,
        mode: RealS3Mode,
        batcher: Arc<SimAsyncBatcher>,
    },
    Real {
        client: Arc<RealS3Client>,
        path: ObjectPath14,
        file_len: u64,
        mode: RealS3Mode,
        handle: tokio::runtime::Handle,
        cache: Mutex<HashMap<u64, Bytes>>,
        inflight: Mutex<HashMap<u64, InflightRx>>,
        gets: AtomicU64,
        bytes: AtomicU64,
        t_start: Mutex<Instant>,
    },
}

/// Unified S3 reader supporting simulated S3, real direct HTTP Range GETs, and real 1 MiB ReadCache.
pub struct BenchS3Reader {
    inner: BenchS3Inner,
}

impl BenchS3Reader {
    pub fn from_bytes(bytes: Bytes, s3_cfg: SimulatedS3Config) -> Arc<Self> {
        let mode = RealS3Mode::from_env();
        if !mode.is_real() {
            let profile = SimS3Profile::effective(mode);
            let (effective_cfg, remote_hint) = match (mode, profile) {
                (RealS3Mode::SimDirect, _) => (
                    SimulatedS3Config::wan_garage().with_direct_mode(DIRECT_COALESCE_GAP_BYTES),
                    true,
                ),
                (_, SimS3Profile::WanGarage) => (SimulatedS3Config::wan_garage(), true),
                (_, SimS3Profile::Default) => (s3_cfg, false),
            };
            let sim = SimulatedS3Reader::from_bytes(bytes, effective_cfg)
                .with_remote_hint(remote_hint);
            return Arc::new(Self {
                inner: BenchS3Inner::Sim {
                    sim: Arc::new(sim),
                    mode,
                    batcher: Arc::new(SimAsyncBatcher::new()),
                },
            });
        }
        let handle = tokio::runtime::Handle::current();
        let client = RealS3Client::global().expect("valid STATAM_REAL_S3 credentials");
        let file_len = bytes.len() as u64;
        let path = run_blocking_on(&handle, client.ensure_uploaded(&bytes))
            .expect("upload benchmark object to S3");
        Arc::new(Self {
            inner: BenchS3Inner::Real {
                client,
                path,
                file_len,
                mode,
                handle,
                cache: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                gets: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
                t_start: Mutex::new(Instant::now()),
            },
        })
    }

    pub fn is_real_s3(&self) -> bool {
        matches!(self.inner, BenchS3Inner::Real { .. })
    }

    pub fn mode(&self) -> RealS3Mode {
        match &self.inner {
            BenchS3Inner::Sim { mode, .. } => *mode,
            BenchS3Inner::Real { mode, .. } => *mode,
        }
    }

    pub fn evict_all(&self) {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.evict_all(),
            BenchS3Inner::Real { cache, .. } => {
                cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
            }
        }
    }

    pub fn reset_counters(&self) {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.reset_counters(),
            BenchS3Inner::Real {
                gets,
                bytes,
                t_start,
                ..
            } => {
                gets.store(0, Ordering::Relaxed);
                bytes.store(0, Ordering::Relaxed);
                *t_start.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
            }
        }
    }

    pub fn counters(&self) -> IoCounters {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.counters(),
            BenchS3Inner::Real { gets, bytes, .. } => IoCounters {
                gets: gets.load(Ordering::Relaxed),
                bytes_from_s3: bytes.load(Ordering::Relaxed),
                bytes_requested: 0,
                cache_hits: 0,
                cache_misses: 0,
                waves: 0,
                wave_latency_ms: 0.0,
            },
        }
    }

    /// Returns simulated latency (ms) in Sim mode, or actual wall-clock elapsed ms since
    /// `reset_counters()` in Real S3 mode.
    pub fn simulated_latency_ms(&self) -> f64 {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.simulated_latency_ms(),
            BenchS3Inner::Real { t_start, .. } => {
                t_start
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .elapsed()
                    .as_secs_f64()
                    * 1000.0
            }
        }
    }

    async fn fetch_raw_range(
        client: &RealS3Client,
        path: &ObjectPath14,
        mode: RealS3Mode,
        gets: &AtomicU64,
        bytes: &AtomicU64,
        range: Range<u64>,
    ) -> Result<Bytes, StatamError> {
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let trace = s3_trace_enabled();
        let t0 = trace.then(Instant::now);
        let range_start = range.start;
        let range_end = range.end;
        let data = client
            .store
            .get_range(path, range)
            .await
            .map_err(|e| StatamError::Io(std::io::Error::other(e.to_string())))?;
        if let Some(t0) = t0 {
            eprintln!(
                "[s3-trace] mode={} start={} end={} len={} elapsed_ms={:.2}",
                mode.label(),
                range_start,
                range_end,
                data.len(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        gets.fetch_add(1, Ordering::Relaxed);
        bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }

    pub async fn read_range_async(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        match &self.inner {
            BenchS3Inner::Sim { sim, batcher, .. } => batcher.read_range(sim, range).await,
            BenchS3Inner::Real {
                client,
                path,
                file_len,
                mode,
                cache,
                gets,
                bytes,
                ..
            } => {
                if range.start > range.end || range.end > *file_len {
                    return Err(StatamError::BoundsExceeded {
                        reason: "real s3 range out of bounds",
                    });
                }
                if range.is_empty() {
                    return Ok(Bytes::new());
                }
                if *mode == RealS3Mode::Direct {
                    return Self::fetch_raw_range(client, path, *mode, gets, bytes, range).await;
                }
                // ReadCache1MiB mode: 1 MiB aligned blocks + contiguous run coalescing
                let _ = cache;
                self.read_ranges_readcache_async(std::slice::from_ref(&range))
                    .await
                    .map(|mut v| v.pop().unwrap_or_default())
            }
        }
    }

    async fn read_ranges_readcache_async(
        &self,
        ranges: &[Range<u64>],
    ) -> Result<Vec<Bytes>, StatamError> {
        let BenchS3Inner::Real {
            client,
            path,
            file_len,
            cache,
            inflight,
            gets,
            bytes,
            ..
        } = &self.inner
        else {
            unreachable!()
        };

        let mut needed_blocks: Vec<u64> = Vec::new();
        for r in ranges {
            if r.start > r.end || r.end > *file_len {
                return Err(StatamError::BoundsExceeded {
                    reason: "real s3 readcache range out of bounds",
                });
            }
            if r.is_empty() {
                continue;
            }
            let first = r.start / S3_CACHE_BLOCK_BYTES;
            let last = (r.end - 1) / S3_CACHE_BLOCK_BYTES;
            for b in first..=last {
                needed_blocks.push(b);
            }
        }
        needed_blocks.sort_unstable();
        needed_blocks.dedup();

        // Single-flight deduplication: concurrent misses to the same 1 MiB block share one GET.
        let mut to_fetch: Vec<(u64, InflightTx)> = Vec::new();
        let mut to_wait: Vec<InflightRx> = Vec::new();
        {
            let cache_guard = cache.lock().unwrap_or_else(|p| p.into_inner());
            let mut inflight_guard = inflight.lock().unwrap_or_else(|p| p.into_inner());
            for &b in &needed_blocks {
                if cache_guard.contains_key(&b) {
                    continue;
                }
                if let Some(rx) = inflight_guard.get(&b) {
                    to_wait.push(rx.clone());
                } else {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    inflight_guard.insert(b, rx);
                    to_fetch.push((b, tx));
                }
            }
        }

        if !to_fetch.is_empty() {
            // Coalesce contiguous missing 1 MiB blocks into runs, then fetch runs concurrently.
            let mut runs: Vec<(u64, u64)> = Vec::new();
            let mut run_start = to_fetch[0].0;
            let mut run_end = to_fetch[0].0;
            for &(b, _) in &to_fetch[1..] {
                if b == run_end + 1 {
                    run_end = b;
                } else {
                    runs.push((run_start, run_end));
                    run_start = b;
                    run_end = b;
                }
            }
            runs.push((run_start, run_end));

            let fetch_res: Result<Vec<(u64, u64, Bytes)>, StatamError> =
                stream::iter(runs.into_iter().map(|(rs, re)| {
                    let byte_start = rs * S3_CACHE_BLOCK_BYTES;
                    let byte_end = ((re + 1) * S3_CACHE_BLOCK_BYTES).min(*file_len);
                    async move {
                        let data = Self::fetch_raw_range(
                            client,
                            path,
                            RealS3Mode::ReadCache1MiB,
                            gets,
                            bytes,
                            byte_start..byte_end,
                        )
                        .await?;
                        Ok::<_, StatamError>((rs, re, data))
                    }
                }))
                .buffer_unordered(MAX_CONCURRENT_GETS)
                .try_collect()
                .await;

            match fetch_res {
                Ok(fetched) => {
                    {
                        let mut cache_guard = cache.lock().unwrap_or_else(|p| p.into_inner());
                        for (rs, re, data) in fetched {
                            for b in rs..=re {
                                let off = ((b - rs) * S3_CACHE_BLOCK_BYTES) as usize;
                                let end = (off + S3_CACHE_BLOCK_BYTES as usize).min(data.len());
                                cache_guard.insert(b, data.slice(off..end));
                            }
                        }
                    }
                    let mut inflight_guard = inflight.lock().unwrap_or_else(|p| p.into_inner());
                    for (b, tx) in to_fetch {
                        inflight_guard.remove(&b);
                        let _ = tx.send(Some(Ok(())));
                    }
                }
                Err(err) => {
                    let msg = err.to_string();
                    let mut inflight_guard = inflight.lock().unwrap_or_else(|p| p.into_inner());
                    for (b, tx) in to_fetch {
                        inflight_guard.remove(&b);
                        let _ = tx.send(Some(Err(msg.clone())));
                    }
                    return Err(err);
                }
            }
        }

        for mut rx in to_wait {
            let res = rx
                .wait_for(|v| v.is_some())
                .await
                .map_err(|_| StatamError::Io(std::io::Error::other("inflight fetch dropped")))?;
            if let Some(Err(msg)) = res.as_ref() {
                return Err(StatamError::Io(std::io::Error::other(msg.clone())));
            }
        }

        let guard = cache.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::with_capacity(ranges.len());
        for r in ranges {
            if r.is_empty() {
                out.push(Bytes::new());
                continue;
            }
            let first = r.start / S3_CACHE_BLOCK_BYTES;
            let last = (r.end - 1) / S3_CACHE_BLOCK_BYTES;
            if first == last {
                let blk = guard.get(&first).ok_or(StatamError::BoundsExceeded {
                    reason: "missing cached block",
                })?;
                let s = (r.start - first * S3_CACHE_BLOCK_BYTES) as usize;
                let e = (r.end - first * S3_CACHE_BLOCK_BYTES) as usize;
                out.push(blk.slice(s..e));
            } else {
                let mut buf = BytesMut::with_capacity((r.end - r.start) as usize);
                for b in first..=last {
                    let blk = guard.get(&b).ok_or(StatamError::BoundsExceeded {
                        reason: "missing cached block",
                    })?;
                    let b_start = b * S3_CACHE_BLOCK_BYTES;
                    let s = (r.start.max(b_start) - b_start) as usize;
                    let e = (r.end.min(b_start + blk.len() as u64) - b_start) as usize;
                    buf.extend_from_slice(&blk[s..e]);
                }
                out.push(buf.freeze());
            }
        }
        Ok(out)
    }

    pub async fn read_ranges_async(
        &self,
        ranges: &[Range<u64>],
    ) -> Result<Vec<Bytes>, StatamError> {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.read_ranges(ranges),
            BenchS3Inner::Real {
                client,
                path,
                file_len,
                mode,
                gets,
                bytes,
                ..
            } => {
                if ranges.is_empty() {
                    return Ok(Vec::new());
                }
                if *mode == RealS3Mode::ReadCache1MiB {
                    return self.read_ranges_readcache_async(ranges).await;
                }
                // Direct mode: coalesce nearby ranges within DIRECT_COALESCE_GAP_BYTES
                // and fetch disjoint spans concurrently (max 16 in-flight).
                for r in ranges {
                    if r.start > r.end || r.end > *file_len {
                        return Err(StatamError::BoundsExceeded {
                            reason: "real s3 direct range out of bounds",
                        });
                    }
                }
                let mut order: Vec<usize> = (0..ranges.len())
                    .filter(|&i| !ranges[i].is_empty())
                    .collect();
                order.sort_by_key(|&i| (ranges[i].start, ranges[i].end));

                let mut spans: Vec<(Range<u64>, Vec<usize>)> = Vec::new();
                for idx in order {
                    let r = &ranges[idx];
                    if let Some((last_span, members)) = spans.last_mut()
                        && r.start <= last_span.end.saturating_add(DIRECT_COALESCE_GAP_BYTES)
                    {
                        last_span.end = last_span.end.max(r.end);
                        members.push(idx);
                    } else {
                        spans.push((r.clone(), vec![idx]));
                    }
                }

                let fetched: Vec<(Range<u64>, Vec<usize>, Bytes)> =
                    stream::iter(spans.into_iter().map(|(span, members)| async move {
                        let data =
                            Self::fetch_raw_range(client, path, *mode, gets, bytes, span.clone())
                                .await?;
                        Ok::<_, StatamError>((span, members, data))
                    }))
                    .buffer_unordered(MAX_CONCURRENT_GETS)
                    .try_collect()
                    .await?;

                let mut out = vec![Bytes::new(); ranges.len()];
                for (span, members, data) in fetched {
                    for idx in members {
                        let r = &ranges[idx];
                        let s = (r.start - span.start) as usize;
                        let e = (r.end - span.start) as usize;
                        out[idx] = data.slice(s..e);
                    }
                }
                Ok(out)
            }
        }
    }
}

impl RangeReader for BenchS3Reader {
    fn file_len(&self) -> u64 {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.file_len(),
            BenchS3Inner::Real { file_len, .. } => *file_len,
        }
    }

    fn is_remote(&self) -> bool {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.is_remote(),
            BenchS3Inner::Real { .. } => true,
        }
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.read_range(range),
            BenchS3Inner::Real { handle, .. } => {
                run_blocking_on(handle, self.read_range_async(range))
            }
        }
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        match &self.inner {
            BenchS3Inner::Sim { sim, .. } => sim.read_ranges(ranges),
            BenchS3Inner::Real { handle, .. } => {
                run_blocking_on(handle, self.read_ranges_async(ranges))
            }
        }
    }
}

/// Async Parquet reader backed by [`BenchS3Reader`], giving Parquet full concurrent
/// `get_byte_ranges` coalescing + Page Index + Bloom Filter pruning on real S3.
#[derive(Clone)]
pub struct BenchS3ParquetAsyncReader {
    reader: Arc<BenchS3Reader>,
    with_page_index: bool,
}

impl BenchS3ParquetAsyncReader {
    pub fn new(reader: Arc<BenchS3Reader>, with_page_index: bool) -> Self {
        Self {
            reader,
            with_page_index,
        }
    }

    pub fn s3(&self) -> &Arc<BenchS3Reader> {
        &self.reader
    }
}

impl AsyncFileReader for BenchS3ParquetAsyncReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        let reader = Arc::clone(&self.reader);
        Box::pin(async move {
            reader
                .read_range_async(range)
                .await
                .map_err(|e| ParquetError::External(Box::new(e)))
        })
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, parquet::errors::Result<Vec<Bytes>>> {
        let reader = Arc::clone(&self.reader);
        Box::pin(async move {
            reader
                .read_ranges_async(&ranges)
                .await
                .map_err(|e| ParquetError::External(Box::new(e)))
        })
    }

    fn get_metadata<'a>(
        &'a mut self,
        _options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        let file_len = self.reader.file_len();
        let with_pi = self.with_page_index;
        Box::pin(async move {
            let policy = if with_pi {
                PageIndexPolicy::Optional
            } else {
                PageIndexPolicy::Skip
            };
            let md_reader = ParquetMetaDataReader::new()
                .with_prefetch_hint(Some(64 * 1024))
                .with_page_index_policy(policy);
            let meta = md_reader.load_and_finish(self, file_len).await?;
            Ok(Arc::new(meta))
        })
    }
}

/// Runs an S3 query once in Sim mode, or adaptively 1..=3 cold runs in Real S3 mode,
/// returning `(matched, gets, bytes, median_wall_ms)`.
pub async fn run_s3_adaptive<F, Fut>(mut run_once: F) -> BenchResult<(usize, u64, u64, f64)>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = BenchResult<(usize, u64, u64, f64)>>,
{
    let mode = RealS3Mode::from_env();
    let max_runs = if !mode.is_real() {
        1
    } else {
        std::env::var("STATAM_S3_RUNS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(3)
    };

    let t0 = Instant::now();
    let (matched, gets, bytes, sim_or_wall) = run_once().await?;
    let first_ms = if mode.is_real() {
        t0.elapsed().as_secs_f64() * 1000.0
    } else {
        sim_or_wall
    };
    if !mode.is_real() || max_runs == 1 || first_ms >= 4_000.0 {
        return Ok((matched, gets, bytes, first_ms));
    }

    let mut samples = Vec::with_capacity(max_runs);
    samples.push(first_ms);
    for _ in 1..max_runs {
        let t = Instant::now();
        let (m, _, _, _) = run_once().await?;
        if m != matched {
            return Err(format!("real S3 non-deterministic row count: {m} vs {matched}").into());
        }
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    Ok((matched, gets, bytes, samples[samples.len() / 2]))
}
