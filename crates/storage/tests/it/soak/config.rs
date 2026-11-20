//! The soak's configuration, read from `CETRA_SOAK_*` (track R M7, R7.5).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use cetra_storage::config::{
    CompactionConfig, QueryConfig, S3Config, StorageConfig, WalConfig, WalSync,
};

use crate::common::s3::S3Env;

/// The file a soak data directory is marked with: only marked directories are emptied.
pub(crate) const MARKER: &str = ".cetra-soak";

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SoakConfig {
    /// `CETRA_SOAK_DIR`, required. The soak deletes it recursively when marked.
    pub(crate) dir: PathBuf,
    /// `CETRA_SOAK_OUT`, required. Missing or empty; never deleted by the soak.
    pub(crate) out: PathBuf,
    /// `CETRA_SOAK_LABEL`, default "local"; names the run.
    pub(crate) label: String,
    /// `CETRA_SOAK_HOURS`, default 24.
    pub(crate) hours: f64,
    /// `CETRA_SOAK_RATE` spans/s, default 2,000.
    pub(crate) rate: u64,
    /// `CETRA_SOAK_PROJECTS`, default 4, 1..=8.
    pub(crate) projects: usize,
    /// `CETRA_SOAK_QPS`, default 1.0.
    pub(crate) qps: f64,
    /// `CETRA_SOAK_QUERY_MIX`, default "list=30,filter=15,detail=25,sessions=10,metrics=10,scores=10".
    pub(crate) mix: Vec<(QueryKind, u32)>,
    /// `CETRA_SOAK_RETENTION_DAYS`, default 0 (keep today only).
    pub(crate) retention_days: u32,
    /// `CETRA_SOAK_SAMPLE_SECS`, default 60.
    pub(crate) sample_secs: u64,
    /// `CETRA_SOAK_WARMUP_HOURS`, default max(1, hours / 10).
    pub(crate) warmup_hours: f64,
    /// `CETRA_SOAK_MAX_RSS_GIB`, default 8.
    pub(crate) max_rss_gib: f64,
    /// `CETRA_SOAK_MAX_L0_AGE_MIN`, default 30.
    pub(crate) max_l0_age_min: u64,
    /// `CETRA_SOAK_KEEP=1`: keep the S3 prefix after a pass.
    pub(crate) keep: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) enum QueryKind {
    List,
    Filter,
    Detail,
    Sessions,
    Metrics,
    Scores,
}

impl QueryKind {
    pub(crate) const ALL: [QueryKind; 6] = [
        QueryKind::List,
        QueryKind::Filter,
        QueryKind::Detail,
        QueryKind::Sessions,
        QueryKind::Metrics,
        QueryKind::Scores,
    ];

    /// The name used in `CETRA_SOAK_QUERY_MIX` and in the samples.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Filter => "filter",
            Self::Detail => "detail",
            Self::Sessions => "sessions",
            Self::Metrics => "metrics",
            Self::Scores => "scores",
        }
    }
}

const DEFAULT_MIX: &str = "list=30,filter=15,detail=25,sessions=10,metrics=10,scores=10";

/// Parses `kind=weight,…`. Every weight is at least 1 and every kind is known.
pub(crate) fn parse_mix(text: &str) -> Result<Vec<(QueryKind, u32)>, String> {
    text.split(',')
        .map(|part| {
            let (name, weight) = part
                .split_once('=')
                .ok_or_else(|| format!("query mix entry without a weight: {part}"))?;
            let kind = QueryKind::ALL
                .into_iter()
                .find(|kind| kind.name() == name.trim())
                .ok_or_else(|| format!("unknown query kind: {}", name.trim()))?;
            let weight: u32 = weight
                .trim()
                .parse()
                .map_err(|_| format!("query weight of {name} is not a number"))?;
            if weight == 0 {
                return Err(format!("query weight of {name} must be at least 1"));
            }
            Ok((kind, weight))
        })
        .collect()
}

impl SoakConfig {
    /// Pure parser, as `S3Env::from_lookup` (R7.1): unit tests pass a map.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let required = |name: &str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} is required"))
        };
        let number = |name: &str| -> Result<Option<f64>, String> {
            lookup(name)
                .map(|value| {
                    value
                        .parse::<f64>()
                        .map_err(|_| format!("{name} is not a number"))
                })
                .transpose()
        };
        let dir = required("CETRA_SOAK_DIR")?;
        if dir.starts_with("/tmp") {
            return Err("CETRA_SOAK_DIR must not be under /tmp (the scratch quota)".to_owned());
        }
        let out = required("CETRA_SOAK_OUT")?;
        let hours = number("CETRA_SOAK_HOURS")?.unwrap_or(24.0);
        let projects = number("CETRA_SOAK_PROJECTS")?.map_or(4, |n| n as usize);
        if !(1..=8).contains(&projects) {
            return Err("CETRA_SOAK_PROJECTS must be between 1 and 8".to_owned());
        }
        let mix = parse_mix(
            lookup("CETRA_SOAK_QUERY_MIX")
                .as_deref()
                .unwrap_or(DEFAULT_MIX),
        )?;
        Ok(Self {
            dir,
            out,
            label: lookup("CETRA_SOAK_LABEL").unwrap_or_else(|| "local".to_owned()),
            hours,
            rate: number("CETRA_SOAK_RATE")?.map_or(2_000, |n| n as u64),
            projects,
            qps: number("CETRA_SOAK_QPS")?.unwrap_or(1.0),
            mix,
            retention_days: number("CETRA_SOAK_RETENTION_DAYS")?.map_or(0, |n| n as u32),
            sample_secs: number("CETRA_SOAK_SAMPLE_SECS")?.map_or(60, |n| n as u64),
            warmup_hours: number("CETRA_SOAK_WARMUP_HOURS")?.unwrap_or((hours / 10.0).max(1.0)),
            max_rss_gib: number("CETRA_SOAK_MAX_RSS_GIB")?.unwrap_or(8.0),
            max_l0_age_min: number("CETRA_SOAK_MAX_L0_AGE_MIN")?.map_or(30, |n| n as u64),
            keep: lookup("CETRA_SOAK_KEEP").as_deref() == Some("1"),
        })
    }

    /// `from_lookup` over the process environment.
    pub(crate) fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

/// Makes `dir` ready. A missing or empty directory is created with the marker
/// file; a marked one is emptied (the marker stays); anything else is refused,
/// so a typo can't wipe real data.
pub(crate) fn prepare_data_dir(dir: &Path) -> Result<(), String> {
    let marker = dir.join(MARKER);
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        return std::fs::write(&marker, b"").map_err(|error| error.to_string());
    }
    let entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    if marker.is_file() {
        for entry in entries {
            let path = entry.path();
            if path == marker {
                continue;
            }
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            removed.map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    if entries.is_empty() {
        return std::fs::write(&marker, b"").map_err(|error| error.to_string());
    }
    Err(format!(
        "{} is not empty and has no {MARKER} marker: refusing to empty it",
        dir.display()
    ))
}

/// The storage config of a soak: the production defaults (spec 03), plus only
/// the data directory, the S3 target, the WAL sync, `min_free_bytes` 2 GiB,
/// compaction every 30 s and a read cache of 10% of the filesystem. `GcConfig`
/// has no interval (only `grace`), so GC keeps its default cadence.
pub(crate) fn storage_config(dir: &Path, s3: Option<&S3Env>, prefix: &str) -> StorageConfig {
    let (object_store, s3_config): (_, S3Config) = match s3 {
        Some(target) => {
            let (url, s3_config) = target.config(prefix);
            (Some(url), s3_config)
        }
        None => (None, S3Config::default()),
    };
    let stat = rustix::fs::statvfs(dir).expect("the data directory exists");
    let read_cache = ByteSize::b(stat.f_blocks * stat.f_frsize / 10);
    StorageConfig {
        delete_objects: true,
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::gib(2),
        object_store,
        s3: s3_config,
        wal: WalConfig {
            sync: WalSync::Always,
            ..WalConfig::default()
        },
        flush: Default::default(),
        memtable: Default::default(),
        query: QueryConfig {
            read_cache_bytes: Some(read_cache),
            ..QueryConfig::default()
        },
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            interval: Duration::from_secs(30),
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}
