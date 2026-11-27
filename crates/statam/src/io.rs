//! Byte-range reader abstraction, local/slice readers, and exact 1 MiB S3 simulator.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};

use crate::StatamError;

/// Exact `ReadCache` alignment unit (1 MiB, matching `crates/storage/src/read_cache.rs`).
pub const S3_CACHE_BLOCK_BYTES: u64 = 1 << 20;

/// Maximum prefetched tail buffer size retained by [`TailCachedReader`] (`1 MiB`).
pub(crate) const MAX_RETAINED_TAIL_BYTES: usize = 1024 * 1024;

/// Abstract synchronous byte-range reader for `.statam` and `.stpk` files.
pub trait RangeReader: Send + Sync {
    /// Total length of the underlying file/object in bytes.
    fn file_len(&self) -> u64;

    /// Reads an exact byte range `[range.start .. range.end)`.
    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError>;

    /// Reads a batch of byte ranges in order.
    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        let mut out = Vec::with_capacity(ranges.len());
        for range in ranges {
            out.push(self.read_range(range.clone())?);
        }
        Ok(out)
    }

    /// Returns `true` when backed by a high-latency remote store (e.g. S3) where
    /// eliminating sequential round-trip waves outweighs small speculative reads
    /// or lightweight concurrency overlap.
    ///
    /// Implementations returning `true` MUST support concurrent `read_range` /
    /// `read_ranges` calls from scoped OS threads (e.g. async bridges must use a
    /// multi-threaded or background reactor, never `tokio::runtime::current_thread`).
    /// Local slice/file readers return `false`.
    fn is_remote(&self) -> bool {
        false
    }
}

/// Wraps a remote [`RangeReader`] with the already-fetched tail buffer `[tail_start..tail_end)`,
/// serving full hits with zero I/O and trimming straddling ranges to only their uncached prefix.
pub(crate) struct TailCachedReader {
    inner: Arc<dyn RangeReader>,
    tail_start: u64,
    tail_end: u64,
    tail: Bytes,
}

impl fmt::Debug for TailCachedReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TailCachedReader")
            .field("file_len", &self.inner.file_len())
            .field("tail_start", &self.tail_start)
            .field("tail_end", &self.tail_end)
            .field("tail_len", &self.tail.len())
            .finish()
    }
}

impl TailCachedReader {
    /// Constructs a [`TailCachedReader`] covering `[tail_start .. tail_start + tail.len())`.
    pub(crate) fn new(
        inner: Arc<dyn RangeReader>,
        tail_start: u64,
        tail: Bytes,
    ) -> Result<Self, StatamError> {
        let tail_end =
            tail_start
                .checked_add(tail.len() as u64)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "tail cache end overflows u64",
                })?;
        if tail_end > inner.file_len() {
            return Err(StatamError::Corrupt {
                reason: "tail cache extends past file_len",
            });
        }
        Ok(Self {
            inner,
            tail_start,
            tail_end,
            tail,
        })
    }

    fn slice_tail(&self, start: u64, end: u64) -> Result<Bytes, StatamError> {
        let rel_s =
            usize::try_from(start - self.tail_start).map_err(|_| StatamError::BoundsExceeded {
                reason: "tail slice start overflows usize",
            })?;
        let rel_e =
            usize::try_from(end - self.tail_start).map_err(|_| StatamError::BoundsExceeded {
                reason: "tail slice end overflows usize",
            })?;
        if rel_s > rel_e || rel_e > self.tail.len() {
            return Err(StatamError::Corrupt {
                reason: "tail slice out of bounds",
            });
        }
        Ok(self.tail.slice(rel_s..rel_e))
    }
}

impl RangeReader for TailCachedReader {
    fn file_len(&self) -> u64 {
        self.inner.file_len()
    }

    fn is_remote(&self) -> bool {
        self.inner.is_remote()
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        validate_range(&range, self.inner.file_len())?;
        if range.start == range.end {
            return Ok(Bytes::new());
        }
        if range.start >= self.tail_start && range.end <= self.tail_end {
            return self.slice_tail(range.start, range.end);
        }
        if range.start < self.tail_start
            && range.end > self.tail_start
            && range.end <= self.tail_end
        {
            let expected_prefix = self.tail_start - range.start;
            let prefix = self.inner.read_range(range.start..self.tail_start)?;
            if prefix.len() as u64 != expected_prefix {
                return Err(StatamError::Corrupt {
                    reason: "short read on straddling tail prefix",
                });
            }
            let suffix = self.slice_tail(self.tail_start, range.end)?;
            let total_len = usize::try_from(range.end - range.start).map_err(|_| {
                StatamError::BoundsExceeded {
                    reason: "straddling range exceeds usize",
                }
            })?;
            let mut buf = BytesMut::with_capacity(total_len);
            buf.extend_from_slice(&prefix);
            buf.extend_from_slice(&suffix);
            return Ok(buf.freeze());
        }
        self.inner.read_range(range)
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        let file_len = self.inner.file_len();
        for r in ranges {
            validate_range(r, file_len)?;
        }
        if ranges.is_empty() {
            return Ok(Vec::new());
        }

        enum SlotPlan {
            Ready(Bytes),
            FullMiss(usize),
            Straddle {
                miss_idx: usize,
                expected_prefix: u64,
                suffix: Bytes,
            },
        }

        let mut plans = Vec::with_capacity(ranges.len());
        let mut misses: Vec<Range<u64>> = Vec::new();
        for r in ranges {
            if r.start == r.end {
                plans.push(SlotPlan::Ready(Bytes::new()));
            } else if r.start >= self.tail_start && r.end <= self.tail_end {
                plans.push(SlotPlan::Ready(self.slice_tail(r.start, r.end)?));
            } else if r.start < self.tail_start && r.end > self.tail_start && r.end <= self.tail_end
            {
                let suffix = self.slice_tail(self.tail_start, r.end)?;
                let miss_idx = misses.len();
                let expected_prefix = self.tail_start - r.start;
                misses.push(r.start..self.tail_start);
                plans.push(SlotPlan::Straddle {
                    miss_idx,
                    expected_prefix,
                    suffix,
                });
            } else {
                let miss_idx = misses.len();
                misses.push(r.clone());
                plans.push(SlotPlan::FullMiss(miss_idx));
            }
        }

        let mut fetched_slots: Vec<Option<Bytes>> = if misses.is_empty() {
            Vec::new()
        } else {
            let fetched = self.inner.read_ranges(&misses)?;
            if fetched.len() != misses.len() {
                return Err(StatamError::Corrupt {
                    reason: "inner read_ranges returned wrong range count",
                });
            }
            fetched.into_iter().map(Some).collect()
        };

        let mut out = Vec::with_capacity(plans.len());
        for plan in plans {
            match plan {
                SlotPlan::Ready(bytes) => out.push(bytes),
                SlotPlan::FullMiss(miss_idx) => {
                    let bytes = fetched_slots[miss_idx].take().ok_or(StatamError::Corrupt {
                        reason: "missing fetched range from inner reader",
                    })?;
                    out.push(bytes);
                }
                SlotPlan::Straddle {
                    miss_idx,
                    expected_prefix,
                    suffix,
                } => {
                    let prefix = fetched_slots[miss_idx].take().ok_or(StatamError::Corrupt {
                        reason: "missing straddling prefix from inner reader",
                    })?;
                    if prefix.len() as u64 != expected_prefix {
                        return Err(StatamError::Corrupt {
                            reason: "short read on straddling tail prefix",
                        });
                    }
                    let total_len = prefix.len().checked_add(suffix.len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "straddling range exceeds usize",
                        },
                    )?;
                    let mut buf = BytesMut::with_capacity(total_len);
                    buf.extend_from_slice(&prefix);
                    buf.extend_from_slice(&suffix);
                    out.push(buf.freeze());
                }
            }
        }
        Ok(out)
    }
}

/// Zero-copy in-memory [`RangeReader`] backed by [`Bytes`].
#[derive(Clone)]
pub struct SliceReader {
    data: Bytes,
}

impl fmt::Debug for SliceReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SliceReader")
            .field("len", &self.data.len())
            .finish()
    }
}

impl SliceReader {
    /// Creates a [`SliceReader`] over `data`.
    pub fn new(data: impl Into<Bytes>) -> Self {
        Self { data: data.into() }
    }

    /// Returns the underlying byte buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl RangeReader for SliceReader {
    fn file_len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        validate_range(&range, self.file_len())?;
        let start = usize::try_from(range.start).map_err(|_| StatamError::BoundsExceeded {
            reason: "range start overflows usize",
        })?;
        let end = usize::try_from(range.end).map_err(|_| StatamError::BoundsExceeded {
            reason: "range end overflows usize",
        })?;
        Ok(self.data.slice(start..end))
    }
}

/// Local file [`RangeReader`] using positional `pread` (`read_exact_at`) on Unix.
#[derive(Debug)]
pub struct FileReader {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    file: Mutex<File>,
    len: u64,
}

impl FileReader {
    /// Opens a file at `path` for range reads.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StatamError> {
        let file = File::open(path)?;
        Self::from_file(file)
    }

    /// Wraps an already-opened [`File`], querying its byte length via `metadata()`.
    pub fn from_file(file: File) -> Result<Self, StatamError> {
        let len = file.metadata()?.len();
        Ok(Self {
            #[cfg(unix)]
            file,
            #[cfg(not(unix))]
            file: Mutex::new(file),
            len,
        })
    }
}

impl RangeReader for FileReader {
    fn file_len(&self) -> u64 {
        self.len
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        validate_range(&range, self.len)?;
        let len_u64 = range.end - range.start;
        if len_u64 == 0 {
            return Ok(Bytes::new());
        }
        let len = usize::try_from(len_u64).map_err(|_| StatamError::BoundsExceeded {
            reason: "read range length overflows usize",
        })?;
        let mut buf = vec![0u8; len];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(&mut buf, range.start)?;
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
            guard.seek(SeekFrom::Start(range.start))?;
            guard.read_exact(&mut buf)?;
        }
        Ok(Bytes::from(buf))
    }
}

/// Configuration for [`SimulatedS3Reader`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimulatedS3Config {
    /// Maximum number of 1 MiB blocks retained in the LRU cache (`0` disables caching).
    pub cache_capacity_blocks: usize,
    /// Simulated per-GET round-trip latency in milliseconds (default: `25.0`).
    pub rtt_ms: f64,
    /// Simulated S3 transfer bandwidth in bytes/second (default: `100 MiB/s`).
    pub bandwidth_bytes_per_sec: f64,
}

impl Default for SimulatedS3Config {
    fn default() -> Self {
        Self {
            cache_capacity_blocks: 256,
            rtt_ms: 25.0,
            bandwidth_bytes_per_sec: 100.0 * 1024.0 * 1024.0,
        }
    }
}

/// I/O telemetry counters recorded by [`SimulatedS3Reader`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoCounters {
    /// Number of coalesced S3 GET requests issued on cache misses.
    pub gets: u64,
    /// Total raw bytes fetched from the backing S3 store (1 MiB aligned, clamped to file length).
    pub bytes_from_s3: u64,
    /// Total logical bytes requested by callers via `read_range` / `read_ranges`.
    pub bytes_requested: u64,
    /// Number of 1 MiB block lookups served from the LRU cache.
    pub cache_hits: u64,
    /// Number of 1 MiB blocks missed in the LRU cache and fetched from S3.
    pub cache_misses: u64,
    /// Number of `read_range` / `read_ranges` invocations (read waves).
    pub waves: u64,
}

impl IoCounters {
    /// Computes simulated latency in milliseconds:
    /// `gets * rtt_ms + (bytes_from_s3 / bandwidth_bytes_per_sec) * 1000.0`.
    pub fn simulated_latency_ms(&self, config: &SimulatedS3Config) -> f64 {
        let rtt_part = (self.gets as f64) * config.rtt_ms;
        let bw_part = if config.bandwidth_bytes_per_sec > 0.0 {
            ((self.bytes_from_s3 as f64) / config.bandwidth_bytes_per_sec) * 1000.0
        } else {
            0.0
        };
        rtt_part + bw_part
    }

    /// Computes simulated latency as a [`Duration`].
    pub fn simulated_latency(&self, config: &SimulatedS3Config) -> Duration {
        let ms = self.simulated_latency_ms(config);
        if ms.is_nan() || ms <= 0.0 {
            Duration::ZERO
        } else if !ms.is_finite() {
            Duration::MAX
        } else {
            Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(Duration::MAX)
        }
    }
}

#[derive(Default)]
struct LruBlockCache {
    capacity: usize,
    seq: u64,
    entries: HashMap<u64, (Bytes, u64)>,
    order: BTreeMap<u64, u64>,
}

impl LruBlockCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seq: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn get(&mut self, block_idx: u64) -> Option<Bytes> {
        let (bytes, old_seq) = self.entries.get_mut(&block_idx)?;
        self.order.remove(old_seq);
        self.seq = self.seq.wrapping_add(1);
        *old_seq = self.seq;
        self.order.insert(self.seq, block_idx);
        Some(bytes.clone())
    }

    fn insert(&mut self, block_idx: u64, bytes: Bytes) {
        if self.capacity == 0 {
            return;
        }
        if let Some((existing, old_seq)) = self.entries.get_mut(&block_idx) {
            self.order.remove(old_seq);
            self.seq = self.seq.wrapping_add(1);
            *old_seq = self.seq;
            *existing = bytes;
            self.order.insert(self.seq, block_idx);
            return;
        }
        while self.entries.len() >= self.capacity {
            if let Some((_, oldest_block)) = self.order.pop_first() {
                self.entries.remove(&oldest_block);
            } else {
                break;
            }
        }
        self.seq = self.seq.wrapping_add(1);
        self.entries.insert(block_idx, (bytes, self.seq));
        self.order.insert(self.seq, block_idx);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.seq = 0;
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

struct S3State {
    cache: LruBlockCache,
    counters: IoCounters,
}

/// [`RangeReader`] simulator modelling Cetra's exact 1 MiB `ReadCache` block
/// rounding, consecutive-run coalescing, LRU eviction, and S3 latency/cost.
pub struct SimulatedS3Reader {
    backing: Arc<dyn RangeReader>,
    config: SimulatedS3Config,
    remote_hint: bool,
    state: Mutex<S3State>,
}

impl fmt::Debug for SimulatedS3Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f.debug_struct("SimulatedS3Reader")
            .field("file_len", &self.backing.file_len())
            .field("config", &self.config)
            .field("remote_hint", &self.remote_hint)
            .field("cached_blocks", &guard.cache.len())
            .field("counters", &guard.counters)
            .finish()
    }
}

impl SimulatedS3Reader {
    /// Wraps `backing` with an exact 1 MiB `ReadCache` and S3 telemetry.
    pub fn new(backing: Arc<dyn RangeReader>, config: SimulatedS3Config) -> Self {
        Self {
            backing,
            config,
            remote_hint: false,
            state: Mutex::new(S3State {
                cache: LruBlockCache::new(config.cache_capacity_blocks),
                counters: IoCounters::default(),
            }),
        }
    }

    /// Sets the value returned by [`RangeReader::is_remote`] (defaults to `false`).
    pub fn with_remote_hint(mut self, is_remote: bool) -> Self {
        self.remote_hint = is_remote;
        self
    }

    /// Convenience constructor over an in-memory byte buffer.
    pub fn from_bytes(data: impl Into<Bytes>, config: SimulatedS3Config) -> Self {
        Self::new(Arc::new(SliceReader::new(data)), config)
    }

    /// Returns a snapshot of the accumulated [`IoCounters`].
    pub fn counters(&self) -> IoCounters {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .counters
    }

    /// Resets all [`IoCounters`] to zero without clearing the block cache.
    pub fn reset_counters(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.counters = IoCounters::default();
    }

    /// Evicts every cached 1 MiB block (e.g. cold-cache or GDPR eviction simulation).
    pub fn evict_all(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.cache.clear();
    }

    /// Number of 1 MiB blocks currently resident in the LRU cache.
    pub fn cached_blocks(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cache
            .len()
    }

    /// Returns the simulated latency in milliseconds for the current [`IoCounters`].
    pub fn simulated_latency_ms(&self) -> f64 {
        self.counters().simulated_latency_ms(&self.config)
    }

    /// Returns the simulated latency as a [`Duration`] for the current [`IoCounters`].
    pub fn simulated_latency(&self) -> Duration {
        self.counters().simulated_latency(&self.config)
    }

    /// Returns the reader's [`SimulatedS3Config`].
    pub fn config(&self) -> &SimulatedS3Config {
        &self.config
    }
}

impl RangeReader for SimulatedS3Reader {
    fn file_len(&self) -> u64 {
        self.backing.file_len()
    }

    fn is_remote(&self) -> bool {
        self.remote_hint
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        let mut results = self.read_ranges(&[range])?;
        results.pop().ok_or(StatamError::Corrupt {
            reason: "missing single range result",
        })
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        let size = self.backing.file_len();
        let mut requested_bytes: u64 = 0;
        for range in ranges {
            validate_range(range, size)?;
            requested_bytes = requested_bytes.saturating_add(range.end - range.start);
        }

        if ranges.is_empty() {
            return Ok(Vec::new());
        }

        let mut indexes: Vec<u64> = ranges
            .iter()
            .filter(|r| r.start < r.end)
            .flat_map(|r| (r.start / S3_CACHE_BLOCK_BYTES)..=((r.end - 1) / S3_CACHE_BLOCK_BYTES))
            .collect();
        indexes.sort_unstable();
        indexes.dedup();

        let mut blocks: BTreeMap<u64, Bytes> = BTreeMap::new();
        let mut missing: Vec<u64> = Vec::new();
        let mut hits: u64 = 0;

        {
            let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.counters.waves = guard.counters.waves.saturating_add(1);
            guard.counters.bytes_requested = guard
                .counters
                .bytes_requested
                .saturating_add(requested_bytes);

            for &idx in &indexes {
                if let Some(block) = guard.cache.get(idx) {
                    blocks.insert(idx, block);
                    hits = hits.saturating_add(1);
                } else {
                    missing.push(idx);
                }
            }
            guard.counters.cache_hits = guard.counters.cache_hits.saturating_add(hits);
        }

        if !missing.is_empty() {
            let runs = coalesce_runs(&missing);
            let mut byte_ranges: Vec<Range<u64>> = Vec::with_capacity(runs.len());
            for &(first, last) in &runs {
                let start =
                    first
                        .checked_mul(S3_CACHE_BLOCK_BYTES)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "read cache run start overflows u64",
                        })?;
                let end = last
                    .checked_add(1)
                    .and_then(|n| n.checked_mul(S3_CACHE_BLOCK_BYTES))
                    .map_or(size, |v| v.min(size));
                byte_ranges.push(start..end);
            }

            let parts = self.backing.read_ranges(&byte_ranges)?;
            if parts.len() != runs.len() {
                return Err(StatamError::Corrupt {
                    reason: "backing reader returned wrong range count",
                });
            }

            let mut fetched: Vec<(u64, Bytes)> = Vec::with_capacity(missing.len());
            let mut fetched_bytes: u64 = 0;

            for (&(first, last), part) in runs.iter().zip(parts.iter()) {
                fetched_bytes = fetched_bytes.saturating_add(part.len() as u64);
                for index in first..=last {
                    let expected = block_len(size, index).ok_or(StatamError::Corrupt {
                        reason: "read cache block lies past file end",
                    })?;
                    let run_off = index
                        .checked_sub(first)
                        .and_then(|d| d.checked_mul(S3_CACHE_BLOCK_BYTES))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "read cache run offset overflows u64",
                        })?;
                    let data = slice_block_from_run(part, run_off, expected)?;
                    fetched.push((index, data));
                }
            }

            let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.counters.cache_misses = guard
                .counters
                .cache_misses
                .saturating_add(missing.len() as u64);
            guard.counters.gets = guard.counters.gets.saturating_add(runs.len() as u64);
            guard.counters.bytes_from_s3 =
                guard.counters.bytes_from_s3.saturating_add(fetched_bytes);
            for (idx, data) in fetched {
                guard.cache.insert(idx, data.clone());
                blocks.insert(idx, data);
            }
        }

        let mut out = Vec::with_capacity(ranges.len());
        for range in ranges {
            if range.start == range.end {
                out.push(Bytes::new());
            } else {
                out.push(assemble_range(range, &blocks)?);
            }
        }
        Ok(out)
    }
}

/// Groups sorted block indexes into inclusive `(first, last)` consecutive runs.
pub fn coalesce_runs(sorted: &[u64]) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &index in sorted {
        match runs.last_mut() {
            Some((_, last)) if last.checked_add(1) == Some(index) => *last = index,
            _ => runs.push((index, index)),
        }
    }
    runs
}

fn validate_range(range: &Range<u64>, file_len: u64) -> Result<(), StatamError> {
    if range.start > range.end || range.end > file_len {
        return Err(StatamError::Corrupt {
            reason: "byte range out of file bounds",
        });
    }
    Ok(())
}

fn block_len(size: u64, index: u64) -> Option<u64> {
    let start = index.checked_mul(S3_CACHE_BLOCK_BYTES)?;
    if start >= size {
        return None;
    }
    Some((size - start).min(S3_CACHE_BLOCK_BYTES))
}

fn slice_block_from_run(part: &Bytes, offset: u64, expected: u64) -> Result<Bytes, StatamError> {
    let start = usize::try_from(offset).map_err(|_| StatamError::BoundsExceeded {
        reason: "run offset overflows usize",
    })?;
    let end = offset
        .checked_add(expected)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or(StatamError::BoundsExceeded {
            reason: "run slice end overflows usize",
        })?;
    if end > part.len() {
        return Err(StatamError::Corrupt {
            reason: "short read from backing store",
        });
    }
    Ok(part.slice(start..end))
}

fn assemble_range(range: &Range<u64>, blocks: &BTreeMap<u64, Bytes>) -> Result<Bytes, StatamError> {
    let first = range.start / S3_CACHE_BLOCK_BYTES;
    let last = (range.end - 1) / S3_CACHE_BLOCK_BYTES;
    if first == last {
        let block = blocks.get(&first).ok_or(StatamError::Corrupt {
            reason: "missing cached block during assembly",
        })?;
        let base = first
            .checked_mul(S3_CACHE_BLOCK_BYTES)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block base offset overflows u64",
            })?;
        return slice_block_from_run(block, range.start - base, range.end - range.start);
    }
    let capacity =
        usize::try_from(range.end - range.start).map_err(|_| StatamError::BoundsExceeded {
            reason: "assembled range exceeds usize",
        })?;
    let mut out = BytesMut::with_capacity(capacity);
    for index in first..=last {
        let block = blocks.get(&index).ok_or(StatamError::Corrupt {
            reason: "missing cached block during multi-block assembly",
        })?;
        let base = index
            .checked_mul(S3_CACHE_BLOCK_BYTES)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block base offset overflows u64",
            })?;
        let from = range.start.max(base) - base;
        let to = range
            .end
            .min(base.saturating_add(block.len() as u64))
            .saturating_sub(base);
        if from > to {
            return Err(StatamError::Corrupt {
                reason: "short cached block during multi-block assembly",
            });
        }
        let part = slice_block_from_run(block, from, to - from)?;
        out.extend_from_slice(&part);
    }
    if out.len() != capacity {
        return Err(StatamError::Corrupt {
            reason: "assembled range length mismatch",
        });
    }
    Ok(out.freeze())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn make_pattern_bytes(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            out.push(((i * 31 + (i >> 8) * 17) & 0xFF) as u8);
        }
        out
    }

    #[test]
    fn simulated_s3_reader_exact_1mib_block_rounding_and_coalescing() {
        // 3.5 MiB object -> 4 blocks (0, 1, 2 full 1 MiB; 3 is 0.5 MiB).
        let mib = S3_CACHE_BLOCK_BYTES as usize;
        let total_len = 3 * mib + mib / 2;
        let data = make_pattern_bytes(total_len);

        let cfg = SimulatedS3Config {
            cache_capacity_blocks: 3,
            rtt_ms: 25.0,
            bandwidth_bytes_per_sec: 100.0 * 1024.0 * 1024.0,
        };
        let s3 = SimulatedS3Reader::from_bytes(data.clone(), cfg);
        assert_eq!(s3.file_len(), total_len as u64);

        // Read spanning end of block 0 and start of block 1 -> coalesced into 1 GET of 2 MiB!
        let r0 = (mib as u64 - 16)..(mib as u64 + 32);
        let got0 = s3.read_range(r0.clone()).unwrap();
        assert_eq!(got0.as_ref(), &data[r0.start as usize..r0.end as usize]);
        let c = s3.counters();
        assert_eq!(c.waves, 1);
        assert_eq!(c.gets, 1);
        assert_eq!(c.cache_hits, 0);
        assert_eq!(c.cache_misses, 2);
        assert_eq!(c.bytes_from_s3, 2 * S3_CACHE_BLOCK_BYTES);
        assert_eq!(c.bytes_requested, 48);
        assert!(s3.simulated_latency_ms() > 25.0);

        // Re-read inside block 1 -> 100% cache hit, 0 GETs!
        let r1 = (mib as u64 + 100)..(mib as u64 + 200);
        let got1 = s3.read_range(r1.clone()).unwrap();
        assert_eq!(got1.as_ref(), &data[r1.start as usize..r1.end as usize]);
        let c = s3.counters();
        assert_eq!(c.waves, 2);
        assert_eq!(c.gets, 1);
        assert_eq!(c.cache_hits, 1);
        assert_eq!(c.bytes_from_s3, 2 * S3_CACHE_BLOCK_BYTES);

        // Non-adjacent multi-range read: block 0 (cached) + block 3 (missing, partial 0.5 MiB) -> 1 GET of 0.5 MiB.
        let r_b0 = 10..20;
        let r_b3 = (3 * mib as u64 + 5)..(total_len as u64 - 10);
        let parts = s3.read_ranges(&[r_b0, r_b3]).unwrap();
        assert_eq!(parts[0].as_ref(), &data[10..20]);
        assert_eq!(parts[1].as_ref(), &data[(3 * mib + 5)..(total_len - 10)]);
        let c = s3.counters();
        assert_eq!(c.waves, 3);
        assert_eq!(c.gets, 2);
        assert_eq!(c.cache_hits, 2);
        assert_eq!(c.cache_misses, 3);
        assert_eq!(c.bytes_from_s3, 2 * S3_CACHE_BLOCK_BYTES + (mib as u64 / 2));

        // Now cache has blocks [0, 1, 3] (capacity = 3). Reading block 2 evicts LRU block 1.
        let r_b2 = (2 * mib as u64 + 7)..(2 * mib as u64 + 19);
        let got2 = s3.read_range(r_b2.clone()).unwrap();
        assert_eq!(got2.as_ref(), &data[r_b2.start as usize..r_b2.end as usize]);
        assert_eq!(s3.cached_blocks(), 3);

        // Block 1 was evicted, so reading it again misses and triggers 1 GET.
        let before_gets = s3.counters().gets;
        let _ = s3.read_range(r1).unwrap();
        assert_eq!(s3.counters().gets, before_gets + 1);

        // Evict all clears cache.
        s3.evict_all();
        assert_eq!(s3.cached_blocks(), 0);
        s3.reset_counters();
        assert_eq!(s3.counters(), IoCounters::default());
    }

    #[test]
    fn simulated_s3_coalesces_disjoint_runs_correctly() {
        // 5 MiB file; request block 0, block 1, and block 3, 4 in one wave -> 2 GETs.
        let mib = S3_CACHE_BLOCK_BYTES as usize;
        let data = make_pattern_bytes(5 * mib);
        let s3 = SimulatedS3Reader::from_bytes(data.clone(), SimulatedS3Config::default());

        let ranges = [
            0..10u64,
            (mib as u64)..(mib as u64 + 10),
            (3 * mib as u64)..(3 * mib as u64 + 10),
            (4 * mib as u64)..(4 * mib as u64 + 10),
        ];
        let parts = s3.read_ranges(&ranges).unwrap();
        for (r, p) in ranges.iter().zip(parts.iter()) {
            assert_eq!(p.as_ref(), &data[r.start as usize..r.end as usize]);
        }
        let c = s3.counters();
        assert_eq!(c.waves, 1);
        assert_eq!(c.gets, 2);
        assert_eq!(c.bytes_from_s3, 4 * S3_CACHE_BLOCK_BYTES);
    }

    #[test]
    fn slice_and_file_readers_validate_bounds_and_empty_ranges() {
        let data = b"hello-statam-world".to_vec();
        let slice_reader = SliceReader::new(data.clone());
        assert_eq!(slice_reader.read_range(5..5).unwrap().len(), 0);
        assert_eq!(slice_reader.read_range(6..12).unwrap().as_ref(), b"statam");
        let reversed_range = Range { start: 10, end: 5 };
        assert!(slice_reader.read_range(reversed_range).is_err());
        assert!(slice_reader.read_range(0..100).is_err());

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&data).unwrap();
        tmp.flush().unwrap();
        let file_reader = FileReader::open(tmp.path()).unwrap();
        assert_eq!(file_reader.file_len(), data.len() as u64);
        assert_eq!(file_reader.read_range(0..0).unwrap().len(), 0);
        assert_eq!(file_reader.read_range(6..12).unwrap().as_ref(), b"statam");
        assert!(file_reader.read_range(0..100).is_err());
    }

    #[test]
    fn m4_coalesce_runs_overflow_latency_clamp_short_assemble_and_failed_miss_accounting() {
        // coalesce_runs handles u64::MAX without overflow panic.
        assert_eq!(
            coalesce_runs(&[u64::MAX - 1, u64::MAX]),
            vec![(u64::MAX - 1, u64::MAX)]
        );
        assert_eq!(
            coalesce_runs(&[u64::MAX, u64::MAX]),
            vec![(u64::MAX, u64::MAX), (u64::MAX, u64::MAX)]
        );

        // Huge finite and +Inf f64 latencies clamp to Duration::MAX without panicking.
        let huge_counters = IoCounters {
            gets: 1,
            ..IoCounters::default()
        };
        let huge_finite_cfg = SimulatedS3Config {
            rtt_ms: 1e30,
            ..SimulatedS3Config::default()
        };
        assert_eq!(
            huge_counters.simulated_latency(&huge_finite_cfg),
            Duration::MAX
        );
        let inf_cfg = SimulatedS3Config {
            rtt_ms: 1e308,
            ..SimulatedS3Config::default()
        };
        assert_eq!(
            IoCounters {
                gets: u64::MAX,
                ..IoCounters::default()
            }
            .simulated_latency(&inf_cfg),
            Duration::MAX
        );

        // assemble_range rejects short first block (from > to) and short middle block (out.len() != capacity).
        let mib = S3_CACHE_BLOCK_BYTES;
        let mut blocks_short_first = BTreeMap::new();
        blocks_short_first.insert(0, Bytes::from_static(&[0u8; 10]));
        blocks_short_first.insert(1, Bytes::from(vec![0u8; mib as usize]));
        assert!(matches!(
            assemble_range(&((mib - 100)..(mib + 10)), &blocks_short_first),
            Err(StatamError::Corrupt { .. })
        ));

        let mut blocks_short_mid = BTreeMap::new();
        blocks_short_mid.insert(0, Bytes::from(vec![0u8; mib as usize]));
        blocks_short_mid.insert(1, Bytes::from_static(&[0u8; 16]));
        blocks_short_mid.insert(2, Bytes::from(vec![0u8; mib as usize]));
        assert!(matches!(
            assemble_range(&(0..(2 * mib + 16)), &blocks_short_mid),
            Err(StatamError::Corrupt { .. })
        ));

        // Failing backing RangeReader must not increment cache_misses.
        struct FailingReader;
        impl RangeReader for FailingReader {
            fn file_len(&self) -> u64 {
                2 * S3_CACHE_BLOCK_BYTES
            }
            fn read_range(&self, _range: Range<u64>) -> Result<Bytes, StatamError> {
                Err(StatamError::Corrupt {
                    reason: "injected backing failure",
                })
            }
        }
        let s3 = SimulatedS3Reader::new(Arc::new(FailingReader), SimulatedS3Config::default());
        assert!(s3.read_range(0..64).is_err());
        assert_eq!(s3.counters().cache_misses, 0);
        assert_eq!(s3.counters().gets, 0);
    }

    #[test]
    fn pass18_tail_cached_reader_hit_miss_straddle_short_read_empty_bounds() {
        struct RecordingReader {
            data: Bytes,
            remote: bool,
            calls: Mutex<Vec<Vec<Range<u64>>>>,
        }
        impl RecordingReader {
            fn new(data: Vec<u8>, remote: bool) -> Self {
                Self {
                    data: Bytes::from(data),
                    remote,
                    calls: Mutex::new(Vec::new()),
                }
            }
            fn take_calls(&self) -> Vec<Vec<Range<u64>>> {
                std::mem::take(&mut *self.calls.lock().unwrap())
            }
        }
        impl RangeReader for RecordingReader {
            fn file_len(&self) -> u64 {
                self.data.len() as u64
            }
            fn is_remote(&self) -> bool {
                self.remote
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                validate_range(&range, self.file_len())?;
                self.calls.lock().unwrap().push(vec![range.clone()]);
                Ok(self.data.slice(range.start as usize..range.end as usize))
            }
            fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
                for r in ranges {
                    validate_range(r, self.file_len())?;
                }
                self.calls.lock().unwrap().push(ranges.to_vec());
                Ok(ranges
                    .iter()
                    .map(|r| self.data.slice(r.start as usize..r.end as usize))
                    .collect())
            }
        }

        // Default is_remote() on SliceReader and SimulatedS3Reader is false; with_remote_hint toggles it.
        let data = make_pattern_bytes(1024);
        assert!(!SliceReader::new(data.clone()).is_remote());
        let sim_default = SimulatedS3Reader::from_bytes(data.clone(), SimulatedS3Config::default());
        assert!(!sim_default.is_remote());
        let sim_remote = SimulatedS3Reader::from_bytes(data.clone(), SimulatedS3Config::default())
            .with_remote_hint(true);
        assert!(sim_remote.is_remote());

        let inner = Arc::new(RecordingReader::new(data.clone(), true));
        // Tail covers [600..1024).
        let tail_start = 600u64;
        let tail = Bytes::copy_from_slice(&data[tail_start as usize..]);

        // Constructor rejects tail extending past file_len.
        assert!(matches!(
            TailCachedReader::new(inner.clone(), 800, tail.clone()),
            Err(StatamError::Corrupt { .. })
        ));

        let cached = TailCachedReader::new(inner.clone(), tail_start, tail.clone()).unwrap();
        assert_eq!(cached.file_len(), 1024);
        assert!(cached.is_remote());

        // 1. Empty range -> 0 inner calls.
        assert!(cached.read_range(100..100).unwrap().is_empty());
        assert!(cached.read_range(700..700).unwrap().is_empty());
        assert!(cached.read_ranges(&[]).unwrap().is_empty());
        assert!(inner.take_calls().is_empty());

        // 2. Bounds errors -> rejected without hitting inner.
        let reversed = Range { start: 50, end: 40 };
        assert!(matches!(
            cached.read_range(reversed.clone()),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            cached.read_range(0..1025),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            cached.read_ranges(&[10..20, reversed]),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            cached.read_ranges(&[10..20, 1000..1025]),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(inner.take_calls().is_empty());

        // 3. Full hit -> 0 inner calls.
        let hit = cached.read_range(600..750).unwrap();
        assert_eq!(hit.as_ref(), &data[600..750]);
        let hit_end = cached.read_range(900..1024).unwrap();
        assert_eq!(hit_end.as_ref(), &data[900..1024]);
        let all_hits = cached
            .read_ranges(&[600..650, 700..700, 950..1024])
            .unwrap();
        assert_eq!(all_hits[0].as_ref(), &data[600..650]);
        assert!(all_hits[1].is_empty());
        assert_eq!(all_hits[2].as_ref(), &data[950..1024]);
        assert!(inner.take_calls().is_empty());

        // 4. Out-of-tail miss -> exact range forwarded to inner.
        let miss = cached.read_range(100..250).unwrap();
        assert_eq!(miss.as_ref(), &data[100..250]);
        assert_eq!(inner.take_calls(), vec![vec![100..250]]);

        // 5. Straddling range in read_range -> trims to [r.start..tail_start) and splices cleanly!
        let straddle = cached.read_range(500..720).unwrap();
        assert_eq!(straddle.as_ref(), &data[500..720]);
        assert_eq!(inner.take_calls(), vec![vec![500..600]]);

        // 6. Mixed hit + miss + straddle + empty batch in read_ranges -> ONE inner call with
        // ONLY the miss and trimmed straddle prefix, scattered back in original order!
        let batch_ranges = [
            10..10u64, // empty
            610..660,  // full hit
            50..120,   // full miss
            550..680,  // straddle -> trimmed to 550..600
            900..1024, // full hit
            580..620,  // second straddle -> trimmed to 580..600
        ];
        let batch_out = cached.read_ranges(&batch_ranges).unwrap();
        assert_eq!(batch_out.len(), batch_ranges.len());
        for (r, got) in batch_ranges.iter().zip(batch_out.iter()) {
            assert_eq!(got.as_ref(), &data[r.start as usize..r.end as usize]);
        }
        assert_eq!(inner.take_calls(), vec![vec![50..120, 550..600, 580..600]]);

        // 7. Short-read check on straddling prefix (both read_range and read_ranges).
        struct ShortPrefixReader {
            file_len: u64,
        }
        impl RangeReader for ShortPrefixReader {
            fn file_len(&self) -> u64 {
                self.file_len
            }
            fn is_remote(&self) -> bool {
                true
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                let len = (range.end - range.start).saturating_sub(1) as usize;
                Ok(Bytes::from(vec![0u8; len]))
            }
        }
        let short_cached =
            TailCachedReader::new(Arc::new(ShortPrefixReader { file_len: 1024 }), 600, tail)
                .unwrap();
        assert!(matches!(
            short_cached.read_range(500..700),
            Err(StatamError::Corrupt {
                reason: "short read on straddling tail prefix"
            })
        ));
        assert!(matches!(
            short_cached.read_ranges(&[610..620, 500..700]),
            Err(StatamError::Corrupt {
                reason: "short read on straddling tail prefix"
            })
        ));
    }
}
