//! Measures an S3-compatible store the way the AnyBlob paper does (Durner, Leis and Neumann,
//! VLDB 2023), to calibrate the simulated S3 of `sim.rs` (`real-data.md` § Real S3).
//!
//! `fmtbench probe s3://bucket/key [reps]` writes one 64 MiB object of random bytes under the
//! key (once), then times ranged GETs at random offsets of it:
//! - by request size, one at a time on a reused connection: time to first byte and total time;
//! - on a fresh client per request (DNS, TCP and TLS each time);
//! - HEAD and LIST, which every cold open issues;
//! - by concurrency, to see when the machine's link, not the connection, is the limit.
//!
//! It ends with the least-squares fit of the median total time against the request size, which
//! gives the simulator's first-byte latency and per-connection throughput.
//!
//! `fmtbench upload <dir> s3://bucket/key [variant...]` copies a local dataset directory under
//! the key, skipping objects that already exist with the same size.

use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};

use crate::R;

const OBJECT_SIZE: u64 = 64 << 20;
const SIZES: &[u64] = &[1 << 10, 4 << 10, 16 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20, 16 << 20];
const CONCURRENCY: &[usize] = &[1, 4, 16, 64, 256];
const CONCURRENT_SIZES: &[u64] = &[64 << 10, 1 << 20, 4 << 20];
/// Bytes read per (size, concurrency) cell of the concurrency sweep, at most.
const CELL_BYTES: u64 = 1 << 30;

/// `s3://bucket/key` → (bucket, key).
pub fn split_url(url: &str) -> R<(String, String)> {
    let rest = url.strip_prefix("s3://").ok_or("expected s3://bucket/key")?;
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    Ok((bucket.to_owned(), key.trim_matches('/').to_owned()))
}

/// A small xorshift generator: offsets only need to be spread, not unpredictable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A random start for a `len`-byte range inside the probe object.
    fn offset(&mut self, len: u64) -> u64 {
        self.next() % (OBJECT_SIZE - len + 1)
    }
}

/// One timed GET: (time to first byte, total time), in ms.
async fn timed_get(store: &dyn ObjectStore, path: &Path, range: std::ops::Range<u64>) -> R<(f64, f64)> {
    let t = Instant::now();
    let opts = GetOptions { range: Some(GetRange::Bounded(range.clone())), ..Default::default() };
    let mut stream = store.get_opts(path, opts).await?.into_stream();
    let mut first = None;
    let mut got = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first.is_none() && !chunk.is_empty() {
            first = Some(t.elapsed());
        }
        got += chunk.len() as u64;
    }
    if got != range.end - range.start {
        return Err(format!("short read: {got} of {} bytes", range.end - range.start).into());
    }
    let total = t.elapsed();
    Ok((ms(first.unwrap_or(total)), ms(total)))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The `p`-th percentile (0–100) of `v`, nearest rank; `v` need not be sorted.
fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

fn dist(v: &[f64]) -> String {
    format!("{:>7.1} {:>7.1} {:>7.1} {:>7.1}", pct(v, 50.0), pct(v, 90.0), pct(v, 99.0), pct(v, 100.0))
}

fn size_label(b: u64) -> String {
    if b >= 1 << 20 { format!("{} MiB", b >> 20) } else { format!("{} KiB", b >> 10) }
}

/// Least squares of `y = a + x * s` over the points; returns (a, s).
fn fit(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in points {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    let s = if den == 0.0 { 0.0 } else { num / den };
    (my - s * mx, s)
}

/// Writes the probe object unless it is already there with the right size.
async fn ensure_object(store: &dyn ObjectStore, path: &Path, rng: &mut Rng) -> R<()> {
    match store.head(path).await {
        Ok(meta) if meta.size == OBJECT_SIZE => return Ok(()),
        Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    let mut data = Vec::with_capacity(OBJECT_SIZE as usize);
    while (data.len() as u64) < OBJECT_SIZE {
        data.extend_from_slice(&rng.next().to_le_bytes());
    }
    let t = Instant::now();
    store.put(path, PutPayload::from(data)).await?;
    println!("wrote the 64 MiB probe object in {:.0} ms", ms(t.elapsed()));
    Ok(())
}

pub async fn probe(url: &str, reps: usize) -> R<()> {
    let (bucket, key) = split_url(url)?;
    let store = crate::s3_store(&bucket)?;
    let prefix = Path::parse(&key)?;
    let path = prefix.clone().join("probe").join("probe-64m.bin");
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64 | 1;
    let mut rng = Rng(seed);
    println!(
        "probe {url}: unix time {}, {reps} requests per size (fewer above 1 MiB), times in ms",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    );
    ensure_object(store.as_ref(), &path, &mut rng).await?;
    for _ in 0..5 {
        let o = rng.offset(4096);
        timed_get(store.as_ref(), &path, o..o + 4096).await?;
    }

    // Sizes, one request at a time, reused connection.
    println!("\nsequential GETs, reused connection");
    println!("{:>8} {:>5}  {:^31}  {:^31}  {:>8}", "size", "n", "first byte p50 p90 p99 max", "total p50 p90 p99 max", "MB/s p50");
    let mut points = Vec::new();
    for &size in SIZES {
        // Large sizes: at most 400 MiB per size, and at least 10 requests.
        let n = reps.min(((400u64 << 20) / size) as usize).max(10);
        let (mut first, mut total, mut rate) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            let o = rng.offset(size);
            let (f, t) = timed_get(store.as_ref(), &path, o..o + size).await?;
            first.push(f);
            total.push(t);
            if t > f {
                rate.push(size as f64 / 1e6 / ((t - f) / 1e3));
            }
        }
        points.push((size as f64, pct(&total, 50.0)));
        let r = if size >= 256 << 10 { format!("{:>8.0}", pct(&rate, 50.0)) } else { format!("{:>8}", "-") };
        println!("{:>8} {n:>5}  {}  {}  {r}", size_label(size), dist(&first), dist(&total));
    }

    // A fresh client per request: what the first request of a cold process pays.
    let mut fresh = Vec::new();
    for _ in 0..reps.min(30) {
        let s = crate::s3_store(&bucket)?;
        let o = rng.offset(4096);
        fresh.push(timed_get(s.as_ref(), &path, o..o + 4096).await?.1);
    }
    println!("\n4 KiB GET on a fresh client (n {}): total p50 p90 p99 max {}", fresh.len(), dist(&fresh));

    let mut heads = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        store.head(&path).await?;
        heads.push(ms(t.elapsed()));
    }
    println!("HEAD (n {}): p50 p90 p99 max {}", heads.len(), dist(&heads));
    let mut lists = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        store.list_with_delimiter(Some(&prefix)).await?;
        lists.push(ms(t.elapsed()));
    }
    println!("LIST with delimiter (n {}): p50 p90 p99 max {}", lists.len(), dist(&lists));

    // Concurrency: requests in flight at once, on the shared client.
    println!("\nconcurrent GETs, shared client");
    println!("{:>8} {:>5} {:>6}  {:^31}  {:>9} {:>9}", "size", "conc", "n", "total p50 p90 p99 max", "MB/s", "req/s");
    let mut best_link = 0.0f64;
    for &size in CONCURRENT_SIZES {
        for &c in CONCURRENCY {
            let n = (4 * c).min((CELL_BYTES / size) as usize).max(c);
            let offsets: Vec<u64> = (0..n).map(|_| rng.offset(size)).collect();
            let store = Arc::clone(&store);
            let t = Instant::now();
            let totals: Vec<f64> = futures::stream::iter(offsets)
                .map(|o| {
                    let (store, path) = (Arc::clone(&store), path.clone());
                    async move { timed_get(store.as_ref(), &path, o..o + size).await.map(|r| r.1) }
                })
                .buffer_unordered(c)
                .try_collect()
                .await?;
            let wall = t.elapsed().as_secs_f64();
            let mbps = (n as u64 * size) as f64 / 1e6 / wall;
            best_link = best_link.max(mbps);
            println!("{:>8} {c:>5} {n:>6}  {}  {mbps:>9.0} {:>9.0}", size_label(size), dist(&totals), n as f64 / wall);
        }
    }

    let (a, s) = fit(&points);
    let conn = if s > 0.0 { 1.0 / s / 1e3 } else { f64::INFINITY };
    println!("\nfit of the median total time over the sizes: {a:.1} ms + size / {conn:.0} MB/s");
    println!(
        "simulator: FMT_SIM_LATENCY_MS={a:.0} FMT_SIM_CONN_MBPS={conn:.0} FMT_SIM_LINK_GBPS={:.1} (best measured aggregate)",
        best_link * 8.0 / 1e3
    );
    Ok(())
}

/// Top-level files of a dataset directory above this size (merged copies) are not uploaded.
const TOP_FILE_MAX: u64 = 16 << 20;

/// Copies the variant subdirectories of `dir` (those named in `only`, or all) and its small
/// top-level files (ids, needles) to `url`, keeping relative paths and following symlinks; skips
/// objects that exist with the same size. Files above 64 MiB go up in 16 MiB parts.
pub async fn upload(dir: &FsPath, url: &str, only: &[String]) -> R<()> {
    let (bucket, key) = split_url(url)?;
    let store = crate::s3_store(&bucket)?;
    let prefix = Path::parse(&key)?;
    // Symlinks are followed: the bench directories link most variants to older copies.
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::metadata(&path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if meta.is_dir() && (only.is_empty() || only.iter().any(|o| *o == name)) {
            let mut stack = vec![path];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d)? {
                    let p = entry?.path();
                    let m = std::fs::metadata(&p)?;
                    if m.is_dir() {
                        stack.push(p);
                    } else if m.is_file() {
                        files.push(p);
                    }
                }
            }
        } else if meta.is_file() && meta.len() <= TOP_FILE_MAX {
            files.push(path);
        }
    }
    files.sort();
    let t = Instant::now();
    let results: Vec<(u64, bool)> = futures::stream::iter(files)
        .map(|file| {
            let store = Arc::clone(&store);
            let rel = file.strip_prefix(dir).map(|p| p.to_string_lossy().into_owned());
            let prefix = prefix.clone();
            async move {
                let rel = rel?;
                let target = rel.split('/').fold(prefix, |p, part| p.join(part));
                let len = std::fs::metadata(&file)?.len();
                match store.head(&target).await {
                    Ok(meta) if meta.size == len => return Ok::<_, Box<dyn std::error::Error + Send + Sync>>((len, false)),
                    Ok(_) | Err(object_store::Error::NotFound { .. }) => {}
                    Err(e) => return Err(e.into()),
                }
                if len <= 64 << 20 {
                    store.put(&target, PutPayload::from(std::fs::read(&file)?)).await?;
                } else {
                    let mut w = WriteMultipart::new_with_chunk_size(store.put_multipart(&target).await?, 16 << 20);
                    let mut f = std::fs::File::open(&file)?;
                    let mut buf = vec![0u8; 16 << 20];
                    loop {
                        let n = std::io::Read::read(&mut f, &mut buf)?;
                        if n == 0 {
                            break;
                        }
                        w.wait_for_capacity(4).await?;
                        w.write(&buf[..n]);
                    }
                    w.finish().await?;
                }
                println!("  {rel} ({} KiB)", len >> 10);
                Ok((len, true))
            }
        })
        .buffer_unordered(8)
        .try_collect()
        .await?;
    let sent: u64 = results.iter().filter(|r| r.1).map(|r| r.0).sum();
    let skipped = results.iter().filter(|r| !r.1).count();
    println!(
        "uploaded {} files, {} MiB in {:.0} s; {skipped} already there",
        results.len() - skipped,
        sent >> 20,
        t.elapsed().as_secs_f64()
    );
    Ok(())
}
