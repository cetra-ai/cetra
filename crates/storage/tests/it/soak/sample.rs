//! The per-minute sample of a soak (track R M7, R7.5): process, ingest, storage
//! and query values, one JSON line each. No field holds a path, a host or a bucket.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, Utc};
use sqlx::{ConnectOptions, Connection};

use super::driver::Shared;
use crate::common::machine::dir_bytes;
use crate::common::s3::S3Env;

/// Close delay of the day-close rule (spec 03 §5), plus 30 minutes of slack.
const OVERDUE_NS: i64 = (2 * 3600 + 30 * 60) * 1_000_000_000;
/// An orphan must be this old (seconds) before it is confirmed.
const ORPHAN_AGE_S: i64 = 600;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Sample {
    pub(crate) t: i64,
    pub(crate) elapsed_s: f64,
    // process
    pub(crate) rss_bytes: u64,
    pub(crate) hwm_bytes: u64,
    /// Anonymous memory in RSS (heap, stacks), `RssAnon`.
    pub(crate) rss_anon_bytes: u64,
    /// File-backed memory in RSS (the binary, mapped files), `RssFile`.
    pub(crate) rss_file_bytes: u64,
    pub(crate) threads: u64,
    pub(crate) fds: u64,
    pub(crate) cpu_s: f64,
    pub(crate) load1: f64,
    // ingest
    pub(crate) target_spans_total: u64,
    pub(crate) appended_spans_total: u64,
    pub(crate) late_spans_total: u64,
    pub(crate) appended_scores_total: u64,
    pub(crate) sampled_traces_total: u64,
    pub(crate) backpressure_total: u64,
    pub(crate) backpressure_wait_ms_total: u64,
    pub(crate) failures_total: BTreeMap<String, u64>,
    // storage
    pub(crate) memtable_bytes: u64,
    pub(crate) wal_failed: bool,
    pub(crate) wal_bytes: u64,
    pub(crate) files: BTreeMap<String, LevelFiles>,
    pub(crate) partitions: u64,
    pub(crate) obsolete_files: u64,
    pub(crate) quarantined_files: u64,
    pub(crate) staged_objects: u64,
    pub(crate) l0_oldest_age_s: f64,
    pub(crate) overdue_partitions: u64,
    pub(crate) retention_violations: u64,
    pub(crate) pending_deletions: u64,
    pub(crate) oldest_pending_deletion_s: f64,
    pub(crate) objects_bytes: u64,
    pub(crate) objects_files: u64,
    pub(crate) orphans: u64,
    pub(crate) free_bytes: u64,
    pub(crate) tmp_bytes: u64,
    pub(crate) cache: Option<CacheSample>,
    // queries since the last sample
    pub(crate) queries: BTreeMap<String, QuerySample>,
    // span appends since the last sample
    pub(crate) appends: AppendSample,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct LevelFiles {
    pub(crate) l0: u64,
    pub(crate) l1: u64,
    pub(crate) l0_bytes: u64,
    pub(crate) l1_bytes: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CacheSample {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) bytes: u64,
    pub(crate) blocks: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct QuerySample {
    pub(crate) n: u64,
    pub(crate) p50_ms: f64,
    pub(crate) p95_ms: f64,
    pub(crate) max_ms: f64,
    pub(crate) errors: u64,
}

/// The span appends of one sample: their latency (backpressure waits included) and how late
/// the ingest ticks that made them fired.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AppendSample {
    pub(crate) n: u64,
    pub(crate) p50_ms: f64,
    pub(crate) p99_ms: f64,
    pub(crate) max_ms: f64,
    pub(crate) tick_late_p99_ms: f64,
    pub(crate) tick_late_max_ms: f64,
}

/// Nanoseconds since the Unix epoch.
pub(crate) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_nanos() as i64
}

/// Suspects and confirmed orphans. A listed object that no catalog row names is
/// a suspect. It is confirmed when it was a suspect in the previous sample too
/// and is older than 10 minutes. `.`-prefixed `.tmp-` names are never suspects.
/// Returns (confirmed count, this sample's suspects).
pub(crate) fn orphans(
    listed: &[(String, i64)],
    known: &HashSet<String>,
    previous: &HashSet<String>,
    now_s: i64,
) -> (u64, HashSet<String>) {
    let mut suspects = HashSet::new();
    let mut confirmed = 0;
    for (path, modified_s) in listed {
        if is_tmp(path) || known.contains(path) {
            continue;
        }
        suspects.insert(path.clone());
        if previous.contains(path) && now_s - modified_s > ORPHAN_AGE_S {
            confirmed += 1;
        }
    }
    (confirmed, suspects)
}

fn is_tmp(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with('.') && name.contains(".tmp-")
}

/// The memory and thread fields of `/proc/self/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcStatus {
    pub(crate) rss_bytes: u64,
    pub(crate) hwm_bytes: u64,
    pub(crate) rss_anon_bytes: u64,
    pub(crate) rss_file_bytes: u64,
    pub(crate) threads: u64,
}

/// Reads [`ProcStatus`] from the text of `/proc/self/status`.
pub(crate) fn parse_status(text: &str) -> ProcStatus {
    let field = |name: &str| -> u64 {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("/proc/self/status has no {name}"))
    };
    ProcStatus {
        rss_bytes: field("VmRSS:") * 1024,
        hwm_bytes: field("VmHWM:") * 1024,
        rss_anon_bytes: field("RssAnon:") * 1024,
        rss_file_bytes: field("RssFile:") * 1024,
        threads: field("Threads:"),
    }
}

/// User plus system CPU seconds from `/proc/self/stat` (USER_HZ is 100).
pub(crate) fn parse_cpu(stat: &str) -> f64 {
    // The command name is in parentheses and may contain spaces: count from the last `)`.
    let after = stat.rsplit(')').next().unwrap_or("");
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: f64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let stime: f64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (utime + stime) / 100.0
}

/// One data file row group of the catalog: a partition and a level.
#[derive(Debug)]
struct GroupRow {
    table: String,
    project: String,
    day: String,
    level: i64,
    count: i64,
    bytes: i64,
    min_created: i64,
    max_created: i64,
}

/// One `data_files` group row as the catalog returns it.
type GroupTuple = (String, String, String, i64, i64, i64, i64, i64);

/// The catalog rows one sample needs, read in one transaction.
struct CatalogRows {
    groups: Vec<GroupRow>,
    states: Vec<(String, i64)>,
    paths: Vec<String>,
    staged: Vec<String>,
    watermarks: Vec<(String, String)>,
    pending: (i64, Option<i64>),
}

/// The read-only catalog connection and the object listing of one soak.
pub(crate) struct Sampler {
    connection: sqlx::SqliteConnection,
    dir: PathBuf,
    /// The S3 target and the run's prefix, when the soak runs on S3.
    s3: Option<(S3Env, String)>,
    start: Instant,
    /// Last sample's orphan suspects.
    suspects: HashSet<String>,
}

impl Sampler {
    /// Opens the catalog read-only. The soak keeps the connection for its whole run.
    pub(crate) async fn open(
        dir: &Path,
        s3: Option<(S3Env, String)>,
        start: Instant,
    ) -> Result<Self, String> {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .busy_timeout(std::time::Duration::from_secs(5));
        // Read-only first. If the `-shm` file cannot be opened read-only while the
        // storage runs, open read-write: the sampler only runs SELECTs.
        let connection = match options.clone().read_only(true).connect().await {
            Ok(connection) => connection,
            Err(_) => options
                .read_only(false)
                .connect()
                .await
                .map_err(|error| error.to_string())?,
        };
        Ok(Self {
            connection,
            dir: dir.to_path_buf(),
            s3,
            start,
            suspects: HashSet::new(),
        })
    }

    /// Objects under the run's tables: (path relative to the objects directory,
    /// modification second), with the count and bytes.
    async fn list_objects(&self) -> Result<(Vec<(String, i64)>, u64, u64), String> {
        match &self.s3 {
            None => {
                let objects = self.dir.join("objects");
                let mut listed = Vec::new();
                let mut bytes = 0;
                let mut stack = vec![objects.join("tables")];
                while let Some(path) = stack.pop() {
                    let Ok(entries) = std::fs::read_dir(&path) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let child = entry.path();
                        if child.is_dir() {
                            stack.push(child);
                            continue;
                        }
                        let Ok(meta) = child.metadata() else {
                            continue;
                        };
                        bytes += meta.len();
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_secs() as i64);
                        if let Ok(relative) = child.strip_prefix(&objects) {
                            listed.push((relative.to_string_lossy().into_owned(), modified));
                        }
                    }
                }
                let count = listed.len() as u64;
                Ok((listed, bytes, count))
            }
            Some((s3, prefix)) => {
                let metas = s3.list(&format!("{prefix}/tables")).await?;
                let strip = format!("{prefix}/");
                let mut bytes = 0;
                let listed: Vec<(String, i64)> = metas
                    .iter()
                    .filter_map(|meta| {
                        bytes += meta.size;
                        let key = meta.location.to_string();
                        let relative = key.strip_prefix(&strip)?.to_owned();
                        Some((relative, meta.last_modified.timestamp()))
                    })
                    .collect();
                let count = listed.len() as u64;
                Ok((listed, bytes, count))
            }
        }
    }

    /// The rows of one transaction, so the sample sees one catalog state.
    async fn catalog(&mut self) -> Result<CatalogRows, String> {
        let mut tx = self
            .connection
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        let groups: Vec<GroupTuple> = sqlx::query_as(
            "SELECT table_name, project_id, day, level, COUNT(*), SUM(size_bytes), MIN(created_at), MAX(created_at)
             FROM data_files WHERE state = 'active' GROUP BY table_name, project_id, day, level",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        let states: Vec<(String, i64)> =
            sqlx::query_as("SELECT state, COUNT(*) FROM data_files GROUP BY state")
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        let paths: Vec<(String,)> = sqlx::query_as("SELECT path FROM data_files")
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
        let staged: Vec<(String,)> = sqlx::query_as("SELECT path FROM staged_objects")
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
        let watermarks: Vec<(String, String)> =
            sqlx::query_as("SELECT project_id, keep_from FROM retention_watermarks")
                .fetch_all(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        let pending: (i64, Option<i64>) = sqlx::query_as(
            "SELECT COUNT(*), MIN(registered_at) FROM deletions WHERE status = 'pending'",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(CatalogRows {
            groups: groups
                .into_iter()
                .map(
                    |(table, project, day, level, count, bytes, min, max)| GroupRow {
                        table,
                        project,
                        day,
                        level,
                        count,
                        bytes,
                        min_created: min,
                        max_created: max,
                    },
                )
                .collect(),
            states,
            paths: paths.into_iter().map(|(path,)| path).collect(),
            staged: staged.into_iter().map(|(path,)| path).collect(),
            watermarks,
            pending,
        })
    }

    /// Orphan suspects of the last sample.
    pub(crate) fn suspect_count(&self) -> usize {
        self.suspects.len()
    }

    /// Takes one sample. Counters are cumulative; query latencies are since the last sample.
    pub(crate) async fn sample(&mut self, shared: &Shared) -> Result<Sample, String> {
        let now = now_ns();
        let now_s = now / 1_000_000_000;
        let (listed, objects_bytes, objects_files) = self.list_objects().await?;
        let rows = self.catalog().await?;
        let mut sample = Sample {
            t: now_s,
            elapsed_s: self.start.elapsed().as_secs_f64(),
            objects_bytes,
            objects_files,
            ..Sample::default()
        };
        derive(&rows, now, &mut sample);

        let known: HashSet<String> = rows.paths.iter().chain(&rows.staged).cloned().collect();
        let (confirmed, suspects) = orphans(&listed, &known, &self.suspects, now_s);
        self.suspects = suspects;
        sample.orphans = confirmed;
        sample.staged_objects = rows.staged.len() as u64;

        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let status = parse_status(&status);
        sample.rss_bytes = status.rss_bytes;
        sample.hwm_bytes = status.hwm_bytes;
        sample.rss_anon_bytes = status.rss_anon_bytes;
        sample.rss_file_bytes = status.rss_file_bytes;
        sample.threads = status.threads;
        sample.fds = std::fs::read_dir("/proc/self/fd").map_or(0, |entries| entries.count() as u64);
        let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
        sample.cpu_s = parse_cpu(&stat);
        sample.load1 = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);

        let health = shared.storage.health();
        sample.memtable_bytes = health.memtable_bytes;
        sample.wal_failed = health.spans_wal_failed || health.scores_wal_failed;
        sample.wal_bytes = dir_bytes(&self.dir.join("wal"));
        sample.cache = shared.storage.read_cache_stats().map(|stats| CacheSample {
            hits: stats.hits,
            misses: stats.misses,
            bytes: stats.bytes,
            blocks: stats.blocks,
        });
        sample.tmp_bytes = dir_bytes(&self.dir.join("tmp"));
        sample.free_bytes =
            rustix::fs::statvfs(&self.dir).map_or(0, |stat| stat.f_bavail * stat.f_frsize);

        let counters = &shared.counters;
        let load = |counter: &std::sync::atomic::AtomicU64| {
            counter.load(std::sync::atomic::Ordering::Relaxed)
        };
        sample.target_spans_total =
            (shared.config.rate as f64 * self.start.elapsed().as_secs_f64()) as u64;
        sample.appended_spans_total = load(&counters.appended_spans);
        sample.late_spans_total = load(&counters.late_spans);
        sample.appended_scores_total = load(&counters.appended_scores);
        sample.sampled_traces_total = load(&counters.sampled_traces);
        sample.backpressure_total = load(&counters.backpressure);
        sample.backpressure_wait_ms_total = load(&counters.backpressure_wait_ms);
        sample.failures_total = shared
            .failures
            .counts()
            .into_iter()
            .map(|(kind, count)| (kind.name(), count))
            .collect();
        sample.queries = shared.take_query_stats();
        sample.appends = shared.take_append_stats();
        Ok(sample)
    }
}

/// Fills the catalog-derived fields of `sample` from `rows` at `now` (ns).
fn derive(rows: &CatalogRows, now: i64, sample: &mut Sample) {
    let today = Utc::now().date_naive();
    let today_text = today.format("%Y-%m-%d").to_string();
    let watermarks: HashMap<&str, NaiveDate> = rows
        .watermarks
        .iter()
        .filter_map(|(project, day)| {
            Some((
                project.as_str(),
                NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?,
            ))
        })
        .collect();
    let mut partitions = HashSet::new();
    let mut l0_oldest: Option<i64> = None;
    // Spans partitions with L0 files whose day has ended: (project, day) → latest L0 write.
    let mut closed: HashMap<(&str, &str), i64> = HashMap::new();
    for row in &rows.groups {
        partitions.insert((row.table.as_str(), row.project.as_str(), row.day.as_str()));
        let level_files = sample.files.entry(row.table.clone()).or_default();
        match row.level {
            0 => {
                level_files.l0 += row.count as u64;
                level_files.l0_bytes += row.bytes as u64;
            }
            1 => {
                level_files.l1 += row.count as u64;
                level_files.l1_bytes += row.bytes as u64;
            }
            _ => {}
        }
        if row.table == "spans" && row.level == 0 {
            if row.day == today_text {
                l0_oldest = Some(l0_oldest.map_or(row.min_created, |m| m.min(row.min_created)));
            } else if row.day < today_text {
                let latest = closed
                    .entry((row.project.as_str(), row.day.as_str()))
                    .or_insert(i64::MIN);
                *latest = (*latest).max(row.max_created);
            }
        }
        if let (Some(keep_from), Ok(day)) = (
            watermarks.get(row.project.as_str()),
            NaiveDate::parse_from_str(&row.day, "%Y-%m-%d"),
        ) && day < *keep_from
        {
            sample.retention_violations += row.count as u64;
        }
    }
    sample.partitions = partitions.len() as u64;
    sample.l0_oldest_age_s = l0_oldest.map_or(0.0, |min| (now - min) as f64 / 1e9);
    sample.overdue_partitions = closed
        .iter()
        .filter(|((_, day), latest)| {
            let end_of_day = NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.succ_opt())
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
                .unwrap_or(i64::MAX);
            now - end_of_day.max(**latest) > OVERDUE_NS
        })
        .count() as u64;
    for (state, count) in &rows.states {
        match state.as_str() {
            "obsolete" => sample.obsolete_files = *count as u64,
            "quarantined" => sample.quarantined_files = *count as u64,
            _ => {}
        }
    }
    let (pending, oldest) = rows.pending;
    sample.pending_deletions = pending as u64;
    sample.oldest_pending_deletion_s = oldest.map_or(0.0, |min| (now - min) as f64 / 1e9);
}
