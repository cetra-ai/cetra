//! Columnar storage for spans and scores: a write-ahead log, an in-memory
//! memtable, Parquet files, and a SQLite catalog, queried through `cetra-query`.
//!
//! See `docs/spec/03-storage.md`. The design and milestones are in
//! `docs/plans/a1-storage/00-design.md`.

#![warn(missing_docs)]

use crate::fs::{File, OpenOptions};
use std::io;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cetra_column::Batch;
use cetra_core::ids::ProjectId;
use chrono::NaiveDate;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::catalog::{Committer, Registered};
use crate::config::{QueryConfig, StorageConfig, WalSync};
use crate::error::{
    AppendError, DeletionError, MediaError, MemtableError, OpenError, QueryError, StorageError,
    WalError,
};
use crate::flush::FlushHandle;
use crate::hooks::{HookPoint, Hooks};
use crate::media::MediaGc;
use crate::memtable::Memtable;
use crate::objects::{Objects, PutSizes};
use crate::read_state::{Published, ReadState};
use crate::verify::VerifyQueue;
use crate::wal::Table;
use crate::wal::writer::{WalWriterHandle, WriterExit, WriterParams, spawn};
use crate::wal::{SegmentWriter, replay};

pub(crate) mod anchor;
pub(crate) mod catalog;
mod compaction;
pub mod config;
pub(crate) mod counters;
pub(crate) mod dedup;
pub(crate) mod deletion;
pub(crate) mod disk;
pub(crate) mod engine;
pub mod error;
pub(crate) mod file_format;
pub(crate) mod flush;
pub(crate) mod format;
pub(crate) mod fs;
pub(crate) mod gc;
mod maintenance;
pub(crate) mod media;
pub(crate) mod memory;
pub(crate) mod objects;
pub(crate) mod partials;
pub(crate) mod projects;
mod query_error;
pub(crate) mod read_cache;
mod reads;
mod snapshot;
mod sql;
pub(crate) mod unwind;
pub(crate) mod verify;
mod views;

pub use crate::compaction::job::CompactionReport;
pub use crate::compaction::rewrite::DeletionReport;
pub use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
pub use crate::error::{
    CompactionError, CopyObjectsError, FlushNowError, OrphanError, ProbeError, SnapshotError,
};
pub use crate::gc::{ObsoleteReport, OrphanReport};
pub use crate::maintenance::Maintenance;

/// The cancel flag of [`Storage::copy_objects`]. A `Mutex` rather than an
/// `AtomicBool`: the copy checks the flag and creates each object's directory and
/// link under the same lock, and a caller sets it under that lock before it removes
/// the destination. An atomic flag alone would let a copy re-create a directory
/// between the caller's removal and its check.
pub type CopyCancel = Arc<Mutex<bool>>;
pub use crate::media::{MediaBlob, MediaRead, MediaReport};
pub use crate::read_cache::ReadCacheStats;
pub use query_error::{InvalidQuery, Position, RunError};
pub(crate) use reads::EngineContext;
pub use reads::{
    ACTIVITY_BUCKETS, Agg, AlertMeasure, AlertValue, AlertValueRequest, Breakdown, Bucket,
    BucketWidth, DEFAULT_BREAKDOWN_LIMIT, ERROR_SPAN_COLUMNS, ERROR_TRACE_COUNT, ErrorSpansRequest,
    ExperimentScore, ExperimentScoreMean, ExperimentTrace, FacetData, FacetField, FacetKind,
    FacetValue, FacetView, FacetsRequest, FieldError, FilterDeletionRequest, FilteredTargets,
    GroupBy, HISTOGRAM_BUCKETS, Histogram, HistogramBucket, LATEST_SCORE_COLUMNS, ListScope,
    MAX_BREAKDOWN_LIMIT, MAX_COUNT_NAMES, MAX_ERROR_SPANS_PER_TRACE, MAX_EXPERIMENT_SCORES,
    MAX_EXPERIMENT_TRACES, MAX_FACET_FIELDS, MAX_FACET_KEY, MAX_FACET_LIMIT, MAX_FACET_QUERY,
    MAX_FILTER_DELETION_TRACES, MAX_MAP_KEY, MAX_MATCH_COUNT, MAX_MEAN_EXPERIMENTS, MAX_MEASURES,
    MAX_PAGE, MAX_PROMPT_VERSIONS, MAX_REFRESH_IDS, MAX_REFRESH_SPANS_PER_TRACE, MAX_SESSION_TURNS,
    MAX_TARGET_IDS, MAX_TRACE_SPANS, MAX_USER_METADATA_KEYS, MAX_USER_SESSIONS, MatchCountRequest,
    Measure, Page, PromptMetricsRequest, PromptScoreMean, PromptVersionMetrics,
    SESSION_ERROR_TRACE_COUNT, SESSION_FIRST_INPUT_PREVIEW, SESSION_LAST_OUTPUT_PREVIEW,
    SPAN_LIST_COLUMNS, SPAN_REFRESH_COLUMNS, ScoreListRequest, ScoreNameCount,
    ScoreNameCountsRequest, ScoreTargets, SessionDetailData, SessionExtrasRequest,
    SessionListRequest, SessionSort, SessionTarget, SessionTargetFilter, Shape, SortOrder,
    SpanListRequest, SpanRefreshRequest, SpanSort, SpanTarget, SpanTargetFilter,
    TRACE_REFRESH_STRICT_COLUMNS, TotalRow, TraceDetailData, TraceListRequest, TraceRefreshRequest,
    TraceSort, TraceSpansData, TraceSpansRequest, TraceTarget, TraceTotals, TraceTotalsRequest,
    USER_COLUMNS, UserDeletionScope, UserDetailData, UserExtras, UserExtrasRequest,
    UserListRequest, UserSort, WidgetData, WidgetError, WidgetPoint, WidgetQuery, WidgetRequest,
    WidgetSeries, WidgetSource, alert_value, error_spans, eval_trace_ids, experiment_score_means,
    experiment_scores, experiment_traces, facets, filter_session_targets, filter_span_targets,
    filter_trace_ids, latest_scores, match_count, prompt_metrics, score_list, score_name_counts,
    scores_by_id, session_detail, session_extras, session_ids_of_traces, session_list,
    session_targets, span_detail, span_list, span_refresh, span_targets, trace_detail, trace_list,
    trace_list_plan, trace_refresh, trace_spans, trace_targets, trace_totals, user_deletion_scope,
    user_detail, user_extras, user_list, validate_expressions, validate_filter, validate_shape,
    widget_query,
};
pub use snapshot::CopyReport;
pub use snapshot::{CatalogSnapshot, LiveObject};
pub use sql::{
    EXPORT_MAX_CHUNK_BYTES, EXPORT_MAX_ROWS, ExportFormat, SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS,
    SqlColumn, SqlJson, SqlRequest,
};
pub use views::{
    ColumnInfo, ColumnKind, FunctionInfo, ProjectQuery, QueryContext, SqliteView, TimeWindow,
    ViewColumns, ViewName, function_catalog, view_catalog,
};

pub(crate) mod hooks;
pub(crate) mod memtable;
pub(crate) mod read_state;
pub(crate) mod wal;

/// Fuzz entry points (track R M3). Not part of the API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzz_entry;

/// Reads Parquet schema column names and total row count for test assertions.
#[doc(hidden)]
pub fn read_parquet_summary_for_test(bytes: bytes::Bytes) -> Result<(Vec<String>, usize), String> {
    use parquet::file::reader::{FileReader as _, SerializedFileReader};
    let reader = SerializedFileReader::new(bytes).map_err(|e| e.to_string())?;
    let file_meta = reader.metadata().file_metadata();
    let names = file_meta
        .schema_descr()
        .root_schema()
        .get_fields()
        .iter()
        .map(|field| field.name().to_owned())
        .collect();
    let rows = usize::try_from(file_meta.num_rows()).map_err(|e| e.to_string())?;
    Ok((names, rows))
}

#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod deletion_tests;
#[cfg(test)]
mod drop_tests;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod quarantine_tests;
#[cfg(test)]
mod retention;
#[cfg(test)]
mod s3_fault_tests;
#[cfg(test)]
mod test_support;

/// One acknowledged append.
///
/// `seq` is the WAL sequence of that record. `rows` is the batch's row count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendReceipt {
    /// WAL sequence number of this record.
    pub seq: u64,
    /// Rows in the batch.
    pub rows: usize,
}

/// What [`Storage::health`] reports. Counters only: this does no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageHealth {
    /// The spans writer has stopped after a WAL error.
    pub spans_wal_failed: bool,
    /// The scores writer has stopped after a WAL error.
    pub scores_wal_failed: bool,
    /// A local object fsync failed. Until a restart, every flush attempt and every
    /// compaction job fails, and GC deletes nothing (D-R-6).
    pub objects_sync_failed: bool,
    /// A `catalog.db` commit failed with an I/O error. Until a restart, every catalog
    /// call fails: flush attempts, compaction and deletion passes, retention, drops and
    /// GC (D-R-6).
    pub catalog_failed: bool,
    /// Accounted bytes of both memtables.
    pub memtable_bytes: u64,
    /// `storage.memtable.hard_limit`. Each table is checked against this on its own.
    pub memtable_hard_limit: u64,
    /// Free disk is under `storage.min_free_bytes`: ingestion is refused (spec 03 §10).
    pub disk_low: bool,
    /// The least free space of the data directory and the local object root, from the last
    /// measurement (at open, then every second).
    pub disk_free_bytes: u64,
    /// `storage.min_free_bytes`.
    pub disk_min_free_bytes: u64,
}

/// Monotonic counters since open, and the WAL size now (`/metrics`, D-N-31).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageStats {
    /// Bytes of WAL segment files on disk: spans, then scores.
    pub wal_bytes: [u64; 2],
    /// Flushes committed, per table (spans, scores).
    pub flushes_ok: [u64; 2],
    /// Flush attempts that failed, per table.
    pub flushes_failed: [u64; 2],
    /// Wall time of committed flushes, nanoseconds, per table.
    pub flush_nanos: [u64; 2],
    /// Compaction jobs committed.
    pub compactions_ok: u64,
    /// Compaction jobs that failed.
    pub compactions_failed: u64,
    /// Compaction jobs deferred for lack of free disk.
    pub compactions_deferred: u64,
    /// Compaction jobs that lost a commit conflict (a later pass redoes them).
    pub compactions_conflict: u64,
    /// Bytes of Parquet objects written by compaction jobs.
    pub compaction_bytes_written: u64,
    /// Obsolete data files and staged objects (including staged media uploads); media blobs collected by media GC are not counted.
    pub gc_objects_deleted: u64,
}

/// What [`Storage::apply_retention`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    /// Files obsoleted, all tables.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
    /// The watermark now stored: the later of the old one and the request.
    pub keep_from: NaiveDate,
}

/// Open data directory: the lock, both writers, and the published memtables.
#[derive(Debug)]
///
/// `Send + Sync`. The server holds this in an `Arc`.
pub struct Storage {
    hard_limit: u64,
    /// `query.trace_lookback` in ns, the padded window around a trace range.
    lookback: i64,
    shutting_down: AtomicBool,
    /// Held for read by every [`Self::append`] until its WAL write returns and
    /// for write by [`Self::drop_project`] across its commit: no append of any
    /// project is between its `is_dropped` check and its WAL write while a
    /// drop commits (spec 03 §7).
    append_gate: tokio::sync::RwLock<()>,
    /// Released by [`Self::shutdown`] so another process can open the directory
    /// while this value is still alive.
    lock: Mutex<Option<File>>,
    /// Published memtables and catalog. `query` loads one snapshot.
    published: Arc<Published>,
    spans: WalWriterHandle,
    scores: WalWriterHandle,
    spans_flush: FlushHandle,
    scores_flush: FlushHandle,
    gc: gc::GcHandle,
    /// Free disk and compaction reservations (spec 03 §10).
    disk: Arc<disk::Disk>,
    disk_monitor: disk::DiskHandle,
    /// Media collection, cloned into the GC and compaction tasks.
    media: MediaGc,
    /// Shared by the background task and [`Self::compact`].
    compactor: Arc<compaction::job::Compactor>,
    compaction: compaction::task::CompactionHandle,
    /// Suspect files (spec 03 §10), shared with the compactor and the anchor rebuild.
    #[cfg_attr(not(test), allow(dead_code))]
    verify: Arc<VerifyQueue>,
    /// Shared with the flush tasks and staged-object collection.
    #[cfg_attr(not(test), allow(dead_code))]
    objects: Arc<Objects>,
    /// Read by [`Self::flush_now`] and the tests. The flush tasks hold their own clones.
    committer: Arc<Committer>,
    /// `wal/spans` and `wal/scores` under the data directory, measured by [`Self::stats`].
    wal_dirs: [PathBuf; 2],
    /// Persistent native query engine runtime shared across queries on this `Storage`.
    qe_runtime: Arc<QeRuntime>,
    /// Records replayed into the memtable at open. A flushed record is not counted.
    #[cfg_attr(not(test), allow(dead_code))]
    replayed: [u64; 2],
    /// Shared with the writers and the flush tasks. Tests arm faults here.
    #[cfg_attr(not(test), allow(dead_code))]
    hooks: Hooks,
}

/// Persistent native query engine runtime shared across all queries on one [`Storage`].
pub(crate) struct QeRuntime {
    pub(crate) admission: Arc<engine::admission::AsyncAdmissionPool>,
    workers: std::sync::OnceLock<Arc<cetra_query::exec::WorkerPool>>,
    workers_init: Mutex<()>,
    pub(crate) metadata_cache: Arc<engine::metadata_cache::MetadataCache>,
    pub(crate) spill: Arc<cetra_query::exec::SpillManager>,
    pub(crate) target_partitions: usize,
    pub(crate) query_memory: QueryMemory,
}

impl QeRuntime {
    /// Lazily initializes and returns the persistent [`cetra_query::exec::WorkerPool`].
    pub(crate) fn workers(
        &self,
    ) -> Result<Arc<cetra_query::exec::WorkerPool>, cetra_query::QueryError> {
        if let Some(pool) = self.workers.get() {
            return Ok(Arc::clone(pool));
        }
        let _guard = self
            .workers_init
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(pool) = self.workers.get() {
            return Ok(Arc::clone(pool));
        }
        let pool = Arc::new(cetra_query::exec::WorkerPool::new(self.target_partitions)?);
        let _ = self.workers.set(Arc::clone(&pool));
        Ok(pool)
    }

    #[cfg(any(test, fuzzing))]
    pub(crate) fn for_test() -> Arc<Self> {
        Arc::new(Self {
            admission: Arc::new(engine::admission::AsyncAdmissionPool::new(64 << 20)),
            workers: std::sync::OnceLock::new(),
            workers_init: Mutex::new(()),
            metadata_cache: Arc::new(engine::metadata_cache::MetadataCache::new(64 << 20)),
            spill: Arc::new(cetra_query::exec::SpillManager::with_fixed_cap(
                PathBuf::new(),
                0,
            )),
            target_partitions: 2,
            query_memory: QueryMemory {
                initial: 1 << 20,
                cap: 64 << 20,
                wait: std::time::Duration::from_secs(1),
            },
        })
    }
}

impl std::fmt::Debug for QeRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QeRuntime")
            .field("target_partitions", &self.target_partitions)
            .field("admission_limit", &self.admission.limit())
            .field("admission_used", &self.admission.used())
            .field("workers_initialized", &self.workers.get().is_some())
            .field("metadata_cache_bytes", &self.metadata_cache.bytes())
            .field("spill", &self.spill)
            .finish()
    }
}

/// What [`Storage::query`] needs to admit a query (spec 03 §6).
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueryMemory {
    /// Bytes a query takes from the shared pool before it starts.
    initial: usize,
    /// Most memory one query may hold.
    cap: usize,
    /// How long a query waits for its initial bytes.
    wait: std::time::Duration,
}

/// Options of one [`Storage::query`] call. Built only with [`QueryOptions::new`], so a caller
/// always states the data window it reads under (D-P-44).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryOptions {
    /// Time hint for a single-trace lookup (`?around=`), ns.
    ///
    /// When a lookup has no `start_time` bound, only files overlapping
    /// `[around - 1 day, around + 1 day]` are read (D154).
    around: Option<i64>,
    /// Rows before this instant (ns) are hidden from the views: spans and traces by
    /// `start_time`, scores by `created_at`. `None` hides nothing (D175, D-P-21).
    visible_from: Option<i64>,
}

impl QueryOptions {
    /// `around`: the single-trace time hint (D154), ns. `visible_from`: the org's data-window
    /// bound (D175, D-P-21), ns; `None` hides nothing.
    pub fn new(around: Option<i64>, visible_from: Option<i64>) -> Self {
        Self {
            around,
            visible_from,
        }
    }

    /// The `around` hint.
    pub fn around(&self) -> Option<i64> {
        self.around
    }

    /// The data-window bound.
    pub fn visible_from(&self) -> Option<i64> {
        self.visible_from
    }
}

impl Storage {
    /// Creates the directory layout, takes `data_dir/LOCK`, replays both WALs
    /// after `flushed_seq`, and starts the writers and flush tasks.
    ///
    /// Returns only after replay, so an append cannot run before recovery.
    /// Replay may push a memtable past `hard_limit`; later appends then answer
    /// [`AppendError::Backpressure`] until a flush makes room.
    pub async fn open(config: StorageConfig) -> Result<Self, OpenError> {
        Self::open_inner(config, None).await
    }

    /// [`Self::open`] with a remote store standing in for S3, uploading with `put_sizes`.
    #[cfg(test)]
    pub(crate) async fn open_with_store(
        config: StorageConfig,
        store: Arc<dyn object_store::ObjectStore>,
        put_sizes: PutSizes,
    ) -> Result<Self, OpenError> {
        Self::open_inner(config, Some((store, put_sizes))).await
    }

    async fn open_inner(
        config: StorageConfig,
        remote: Option<(Arc<dyn object_store::ObjectStore>, PutSizes)>,
    ) -> Result<Self, OpenError> {
        if !config.delete_objects {
            tracing::warn!("object deletion is off (restore mode)");
        }
        let data_dir = config.data_dir.clone();
        let hard_limit = config.memtable.hard_limit.0;
        // `validate` rejects 0. A missed check still yields one partition.
        let target_partitions =
            NonZeroUsize::new(config.query.target_partitions).unwrap_or(NonZeroUsize::MIN);
        let tune = WriterTune {
            sync: config.wal.sync,
            group_max_bytes: config.wal.group_max_bytes.0,
            group_max_wait: config.wal.group_max_wait,
            segment_bytes: config.wal.segment_bytes.0,
            flush_max_bytes: config.flush.max_bytes.0,
        };
        let max_age = config.flush.max_age;
        let upload_concurrency = config.flush.upload_concurrency;
        // The padded trace window uses `query.trace_lookback` itself (D90, D86).
        let lookback = i64::try_from(config.query.trace_lookback.as_nanos()).unwrap_or(i64::MAX);
        // Resolve the anchor window once; `validate` has bounded both sources.
        let window = config.anchor.window.unwrap_or(config.query.trace_lookback);
        let window_ns = i64::try_from(window.as_nanos()).unwrap_or(i64::MAX);
        #[allow(clippy::default_constructed_unit_structs)]
        let hooks = Hooks::default();
        let (lock, committer, mut objects) = open_shared(&config, &hooks, remote).await?;
        let committer = Arc::new(committer);
        let spill_dir = data_dir.join("tmp").join("query");
        let spill_for_cleanup = spill_dir.clone();
        let removed = tokio::task::spawn_blocking(move || remove_spill_files(&spill_for_cleanup))
            .await
            .map_err(|_| {
                open_io(
                    "removing query spill files",
                    &spill_dir,
                    io::Error::other("cleanup task panicked"),
                )
            })??;
        if removed > 0 {
            tracing::info!(removed, "removed leftover query spill files");
        }
        let mut disk_paths = vec![data_dir.clone()];
        disk_paths.extend(objects.local_root().map(Path::to_path_buf));
        let disk = Arc::new(disk::Disk::open(disk_paths, config.min_free_bytes.0)?);
        // The read cache is for remote stores only: local files are on disk already.
        if objects.local_root().is_none() {
            let capacity = read_cache_capacity(&config.query, &data_dir)?;
            if capacity > 0 {
                let cache_dir = data_dir.join("cache");
                let cache_disk = Arc::clone(&disk);
                let min_free = config.min_free_bytes.0;
                let cache = tokio::task::spawn_blocking(move || {
                    read_cache::ReadCache::open(cache_dir, capacity, cache_disk, min_free)
                })
                .await
                .map_err(|_| {
                    open_io(
                        "opening read cache",
                        &data_dir,
                        io::Error::other("open task panicked"),
                    )
                })??;
                objects = objects.with_read_cache(Arc::new(cache));
            }
        }
        let objects = Arc::new(objects);
        let objects_for_cleanup = Arc::clone(&objects);
        let removed = tokio::task::spawn_blocking(move || objects_for_cleanup.remove_temp_files())
            .await
            .map_err(|_| {
                open_io(
                    "removing temporary object files",
                    &data_dir,
                    io::Error::other("cleanup task panicked"),
                )
            })??;
        if removed > 0 {
            tracing::info!(removed, "removed leftover object temp files");
        }
        let (query_memory, total_memory) = resolve_query_memory(&config.query);
        let budget = memory::budget_bytes(&config, u64::try_from(total_memory).unwrap_or(u64::MAX));
        let detected = memory::detected_memory();
        if budgets_exceed_memory(budget, detected) {
            tracing::warn!(
                budget,
                detected,
                "the storage memory budgets exceed the memory limit; the process can be killed under load (spec 03 § 10, Resource needs)"
            );
        }
        let qe_runtime = {
            let admission = Arc::new(engine::admission::AsyncAdmissionPool::new(total_memory));
            let disk_for_spill = Arc::clone(&disk);
            let spill = Arc::new(cetra_query::exec::SpillManager::new(
                spill_dir,
                Arc::new(move || disk_for_spill.engine_spill_cap()),
            ));
            disk.set_engine_spilled(spill.spilled_bytes_counter());
            let metadata_cache_bytes =
                usize::try_from(config.query.metadata_cache_bytes.0).unwrap_or(usize::MAX);
            let metadata_cache = Arc::new(engine::metadata_cache::MetadataCache::new(
                metadata_cache_bytes,
            ));
            Arc::new(QeRuntime {
                admission,
                workers: std::sync::OnceLock::new(),
                workers_init: Mutex::new(()),
                metadata_cache,
                spill,
                target_partitions: target_partitions.get(),
                query_memory,
            })
        };
        let spans_flushed = committer.flushed_seq(Table::Spans).await;
        let scores_flushed = committer.flushed_seq(Table::Scores).await;
        let replay_dir = data_dir.clone();
        let skip_corrupt = config.wal.skip_corrupt;
        let segment_bytes = config.wal.segment_bytes.0;
        let replay_hooks = share_hooks(&hooks);
        let recovered = tokio::task::spawn_blocking(move || {
            recover_tables(
                &replay_dir,
                spans_flushed,
                scores_flushed,
                skip_corrupt,
                segment_bytes,
                &replay_hooks,
            )
        })
        .await
        .map_err(|_| {
            open_io(
                "replaying WAL",
                &data_dir,
                io::Error::other("replay task panicked"),
            )
        })??;
        let catalog = committer.catalog().await;
        let tombstones = committer.tombstones().await;
        let projects = Arc::new(committer.project_states().await?);
        let verify = Arc::new(VerifyQueue::default());
        let mut anchors = anchor::Anchors::new(window_ns);
        anchors.rebuild(&catalog, &objects, &verify).await?;
        let anchors = Arc::new(Mutex::new(anchors));
        let published = Arc::new(Published::new(ReadState {
            spans: recovered.spans.memtable.snapshot(),
            scores: recovered.scores.memtable.snapshot(),
            catalog,
            tombstones,
            projects,
        }));
        // Compaction gets a memory pool of its own, independent of the query runtime's.
        let compactor = {
            let memory = usize::try_from(config.compaction.max_memory.0).unwrap_or(usize::MAX);
            let compaction_pool = cetra_column::GlobalPool::new(memory);
            Arc::new(compaction::job::Compactor::new(
                Arc::clone(&committer),
                Arc::clone(&objects),
                Arc::clone(&disk),
                Arc::clone(&verify),
                Arc::clone(&published),
                compaction::merge::MergeContext::new(
                    8192,
                    config.compaction.threads,
                    compaction_pool,
                    Arc::clone(&qe_runtime),
                    objects.store(),
                    Arc::clone(&verify),
                    tokio::runtime::Handle::current(),
                ),
                compaction::plan::Policy::from(&config.compaction),
                compaction::job::CompactorLimits {
                    threads: config.compaction.threads,
                    max_delay: i64::try_from(config.deletion.max_delay.as_nanos())
                        .unwrap_or(i64::MAX),
                },
                share_hooks(&hooks),
            ))
        };
        let spans = spawn_writer(
            Table::Spans,
            &data_dir,
            recovered.spans,
            &tune,
            &published,
            &hooks,
        );
        let scores = spawn_writer(
            Table::Scores,
            &data_dir,
            recovered.scores,
            &tune,
            &published,
            &hooks,
        );
        let spans_flush = flush::spawn(flush::FlushStart {
            table: Table::Spans,
            dir: data_dir.join("wal").join(Table::Spans.name()),
            shared: Arc::clone(spans.shared()),
            committer: Arc::clone(&committer),
            objects: Arc::clone(&objects),
            published: Arc::clone(&published),
            hooks: share_hooks(&hooks),
            max_bytes: tune.flush_max_bytes,
            max_age,
            upload_concurrency,
            anchors: Some(Arc::clone(&anchors)),
        });
        let scores_flush = flush::spawn(flush::FlushStart {
            table: Table::Scores,
            dir: data_dir.join("wal").join(Table::Scores.name()),
            shared: Arc::clone(scores.shared()),
            committer: Arc::clone(&committer),
            objects: Arc::clone(&objects),
            published: Arc::clone(&published),
            hooks: share_hooks(&hooks),
            max_bytes: tune.flush_max_bytes,
            max_age,
            upload_concurrency,
            anchors: None,
        });
        if let Err(error) =
            gc::collect_once(&committer, &objects, &hooks, catalog::now_ns(), gc::MAX_AGE).await
        {
            // The label only: the error names a staged path, which can be a
            // media path (spec 03 §9).
            tracing::warn!("staged object collection failed");
            tracing::debug!(error = error.label(), "staged object collection error");
        }
        if let Err(error) = gc::collect_obsolete(
            &committer,
            &objects,
            &hooks,
            catalog::now_ns(),
            config.gc.grace,
        )
        .await
        {
            // The label only: the error names an object key.
            tracing::warn!("obsolete file collection failed");
            tracing::debug!(error = error.label(), "obsolete file collection error");
        }
        let media = MediaGc::new(
            Arc::clone(&committer),
            Arc::clone(&objects),
            Arc::clone(&published),
            target_partitions,
            lookback,
            share_hooks(&hooks),
            Arc::clone(&verify),
            Arc::clone(&qe_runtime),
            tokio::runtime::Handle::current(),
        );
        // Finish drops interrupted by a crash or a failed media step, after
        // the first obsolete-file collection (spec 03 §7).
        media.finish_drops().await;
        let gc = gc::spawn(gc::GcStart {
            committer: Arc::clone(&committer),
            objects: Arc::clone(&objects),
            hooks: share_hooks(&hooks),
            grace: config.gc.grace,
            media: MediaGc::clone(&media),
        });
        let disk_monitor = disk::spawn(Arc::clone(&disk), objects.read_cache().map(Arc::clone));
        let compaction = compaction::task::spawn(
            Arc::clone(&compactor),
            MediaGc::clone(&media),
            config.compaction.interval,
        );
        Ok(Self {
            hard_limit,
            lookback,
            shutting_down: AtomicBool::new(false),
            append_gate: tokio::sync::RwLock::new(()),
            lock: Mutex::new(Some(lock)),
            published,
            spans,
            scores,
            spans_flush,
            scores_flush,
            gc,
            disk,
            disk_monitor,
            media,
            compactor,
            compaction,
            verify,
            objects,
            committer,
            wal_dirs: [
                data_dir.join("wal").join(Table::Spans.name()),
                data_dir.join("wal").join(Table::Scores.name()),
            ],
            qe_runtime,
            replayed: [recovered.spans_records, recovered.scores_records],
            hooks,
        })
    }

    /// Appends a spans batch. `Ok` means the record is durable per `wal.sync`
    /// and visible to the next read of the published memtable.
    pub async fn append_spans(
        &self,
        project: ProjectId,
        batch: Batch,
    ) -> Result<AppendReceipt, AppendError> {
        self.append(Table::Spans, project, batch).await
    }

    /// Appends a scores batch. Same durability rule as [`Self::append_spans`].
    pub async fn append_scores(
        &self,
        project: ProjectId,
        batch: Batch,
    ) -> Result<AppendReceipt, AppendError> {
        self.append(Table::Scores, project, batch).await
    }

    /// Admits a query against the shared admission pool, then returns a
    /// [`ProjectQuery`] over the newest published snapshot for `project`.
    ///
    /// Waits up to `query.admission_wait` for `query.initial_memory`; then fails
    /// with [`QueryError::Busy`]. The snapshot is loaded after admission, so a
    /// query that waited sees the newest data.
    ///
    /// The snapshot's pending deletions hide their rows from every read
    /// (spec 03 §7), and partitions older than the project's retention
    /// watermark are excluded (spec 03 §8).
    pub async fn query(
        &self,
        project: ProjectId,
        options: QueryOptions,
    ) -> Result<ProjectQuery, QueryError> {
        let engine = EngineContext::admit_from_storage(self, project, &options).await?;
        Ok(ProjectQuery::from_engine(engine))
    }

    /// Registers a `traces` or `user` deletion. Idempotent by `deletion.id`.
    /// Rows in scope are hidden from every query once this returns (spec 03 §7).
    pub async fn register_deletion(&self, deletion: Deletion) -> Result<(), DeletionError> {
        let published = Arc::clone(&self.published);
        let registered = self
            .committer
            .register_deletion(&deletion, catalog::now_ns(), move |tombstones| {
                read_state::publish_tombstones(&published, tombstones);
            })
            .await?;
        if let Registered::New { seq } = registered {
            // Never log the scope: its ids name the person the deletion is about.
            tracing::info!(
                deletion_id = %deletion.id,
                project = %deletion.project,
                seq,
                "deletion registered"
            );
        }
        Ok(())
    }

    /// `Pending` or `Applied`. An unknown id is [`DeletionError::NotFound`].
    pub async fn deletion_status(&self, id: Uuid) -> Result<DeletionStatus, DeletionError> {
        self.committer
            .deletion_status(id)
            .await?
            .ok_or(DeletionError::NotFound)
    }

    /// Drops every partition of `project` before `keep_from` (a UTC day), and
    /// drops late rows for those days from then on (spec 03 §8). A `keep_from`
    /// earlier than the stored watermark changes nothing.
    pub async fn apply_retention(
        &self,
        project: ProjectId,
        keep_from: NaiveDate,
    ) -> Result<RetentionReport, StorageError> {
        let published = Arc::clone(&self.published);
        let report = self
            .committer
            .apply_retention(project, keep_from, move |next, watermark| {
                published.replace(|state| ReadState {
                    catalog: Arc::clone(next),
                    projects: Arc::new(state.projects.with_keep_from(project, watermark)),
                    ..state.clone()
                });
            })
            .await?;
        if report.files > 0 {
            tracing::info!(
                project = %project,
                keep_from = %report.keep_from,
                files = report.files,
                bytes = report.bytes,
                "retention applied"
            );
        }
        Ok(report)
    }

    /// Hides the project at once and refuses its appends, then deletes its
    /// media. Its files are obsoleted in the same commit and removed by GC.
    /// Idempotent. An error after the commit leaves the project dropped; GC
    /// finishes the media.
    pub async fn drop_project(&self, project: ProjectId) -> Result<(), StorageError> {
        // No append of any project runs between its `is_dropped` check and its
        // WAL write while the drop commits (spec 03 §7).
        let gate = self.append_gate.write().await;
        let published = Arc::clone(&self.published);
        let dropped = self
            .committer
            .drop_project(project, catalog::now_ns(), move |catalog, tombstones| {
                published.replace(|state| ReadState {
                    catalog: Arc::clone(catalog),
                    tombstones: Arc::clone(tombstones),
                    projects: Arc::new(state.projects.with_dropped(project)),
                    ..state.clone()
                });
            })
            .await?;
        drop(gate);
        // A second call finishes a drop whose media step failed, whether the
        // commit above was the first or a no-op. While a catalog snapshot (a
        // backup) holds the gate, the media step is left to the GC task's
        // `finish_drops`, so this call does not wait for the whole backup
        // (N M2, D-N-18).
        if self.committer.object_gc_gate.try_read().is_ok() {
            self.media.finish_drop(project).await?;
        } else {
            tracing::info!(project = %project, "project media deletion deferred to GC");
        }
        if dropped {
            tracing::info!(project = %project, "project dropped");
        }
        Ok(())
    }

    /// Stores `blob` under `media/<project>/<sha256>` once (spec 03 §9, D-A1-6).
    /// Writing an existing hash refreshes its `last_seen_at` and writes nothing
    /// else. A dropped project is refused (spec 03 §7).
    ///
    /// The blob's object is durable before this returns; its `media` row and
    /// the staged row's removal share one transaction.
    pub async fn put_media(&self, project: ProjectId, blob: MediaBlob) -> Result<(), MediaError> {
        if self.published.load().projects.is_dropped(project) {
            return Err(MediaError::ProjectDropped);
        }
        if self.disk.low() {
            return Err(MediaError::DiskFull);
        }
        let now = catalog::now_ns();
        if blob.mime_type.is_empty() || blob.mime_type.len() > 255 || !blob.mime_type.is_ascii() {
            return Err(MediaError::Invalid {
                reason: "mime type",
            });
        }
        if blob.bytes.is_empty() {
            return Err(MediaError::Invalid {
                reason: "empty blob",
            });
        }
        let digest = Sha256::digest(blob.bytes.as_ref());
        if digest.as_slice() != blob.sha256.as_slice() {
            return Err(MediaError::HashMismatch);
        }
        let sha = hex::encode(blob.sha256);
        if self
            .committer
            .touch_media(project, &sha, now, blob.newest_span_start)
            .await?
        {
            return Ok(());
        }
        let path = Objects::media_path(project, &sha);
        self.committer.stage_media(&path, now).await?;
        let size = u64::try_from(blob.bytes.len()).unwrap_or(u64::MAX);
        self.objects.put_media(&path, blob.bytes).await?;
        self.hooks.pause(HookPoint::MediaAfterPut);
        if self.hooks.fail(HookPoint::MediaAfterPut).is_some() {
            return Err(MediaError::Fault);
        }
        // A put that passed its `is_dropped` check before a drop committed is
        // caught here (spec 03 §7); the staged row stays for GC.
        if !self
            .committer
            .insert_media(
                project,
                &sha,
                &blob.mime_type,
                size,
                &path,
                now,
                blob.newest_span_start,
            )
            .await?
        {
            return Err(MediaError::ProjectDropped);
        }
        Ok(())
    }

    /// The blob of `sha256`, whole or `range` of it (end exclusive), with its
    /// MIME type and the whole blob's size (spec 03 §9). A dropped project has
    /// nothing to read (spec 03 §7). `visible_from` hides a blob whose newest
    /// span is older (D-P-21).
    pub async fn get_media(
        &self,
        project: ProjectId,
        sha256: &[u8; 32],
        range: Option<Range<u64>>,
        visible_from: Option<i64>,
    ) -> Result<MediaRead, MediaError> {
        if self.published.load().projects.is_dropped(project) {
            return Err(MediaError::NotFound);
        }
        let sha = hex::encode(sha256);
        let Some((mime_type, size, newest_span_start)) =
            self.committer.media_row(project, &sha).await?
        else {
            return Err(MediaError::NotFound);
        };
        if visible_from.is_some_and(|from| newest_span_start < from) {
            return Err(MediaError::NotFound);
        }
        if let Some(range) = &range
            && (range.start >= range.end || range.end > size)
        {
            return Err(MediaError::InvalidRange);
        }
        let path = Objects::media_path(project, &sha);
        let bytes = self.objects.get_range(&path, range).await?;
        Ok(MediaRead {
            mime_type,
            size,
            bytes,
        })
    }

    /// Read-cache counters, `None` without a read cache (spec 03 §6). Used by
    /// the benchmarks.
    #[doc(hidden)]
    pub fn read_cache_stats(&self) -> Option<ReadCacheStats> {
        self.objects.read_cache().map(|cache| cache.stats())
    }

    /// Writer flags, memtable accounting and free disk. Does not touch the disk: the
    /// free bytes are the last measurement.
    pub fn health(&self) -> StorageHealth {
        let spans = self.spans.shared().memtable_bytes();
        let scores = self.scores.shared().memtable_bytes();
        StorageHealth {
            spans_wal_failed: self.spans.shared().failed(),
            scores_wal_failed: self.scores.shared().failed(),
            objects_sync_failed: self.objects.sync_failed(),
            catalog_failed: self.committer.stopped(),
            memtable_bytes: spans.saturating_add(scores),
            memtable_hard_limit: self.hard_limit,
            disk_low: self.disk.low(),
            disk_free_bytes: self.disk.free(),
            disk_min_free_bytes: self.disk.min_free(),
        }
    }

    /// HEADs the marker `_probe/readyz` and, if that fails, writes it (D-N-36): the store
    /// answered and accepted the credentials. Needs no `s3:ListBucket`. No timeout here; the
    /// caller sets one. Writes the marker even when object deletion is off: it adds an object,
    /// it deletes none.
    pub async fn probe_objects(&self) -> Result<(), ProbeError> {
        self.objects.probe().await
    }

    /// Monotonic counters since open, and the WAL size now (`/metrics`, D-N-31). The WAL
    /// files are measured on a blocking task; a file or directory that cannot be read counts
    /// as 0.
    pub async fn stats(&self) -> StorageStats {
        let mut stats = self.committer.counters().snapshot();
        let dirs = self.wal_dirs.clone();
        stats.wal_bytes =
            tokio::task::spawn_blocking(move || [wal_bytes(&dirs[0]), wal_bytes(&dirs[1])])
                .await
                .unwrap_or_default();
        stats
    }

    /// Flushes both tables up to what was acknowledged before this call and
    /// waits for the catalog commit (N M2, D-N-18). Appends continue meanwhile;
    /// rows acknowledged after the call are not waited for.
    ///
    /// One caller at a time: the server's backup slot serialises calls, and
    /// returning clears both tables' force flags.
    ///
    /// # Errors
    ///
    /// [`FlushNowError`] when a WAL writer has failed, when the storage is
    /// shutting down, or when a table's flush has not committed within `timeout`.
    pub async fn flush_now(&self, timeout: std::time::Duration) -> Result<(), FlushNowError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(FlushNowError::ShuttingDown);
        }
        let _clear = ForceFlushGuard(self);
        // The target is the highest seq held when the call starts. It is a
        // one-shot snapshot: rows appended later are never waited for.
        let mut targets: Vec<(Table, u64)> = Vec::with_capacity(2);
        for table in [Table::Spans, Table::Scores] {
            let shared = self.writer(table).shared();
            let target = shared
                .memtable()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .max_seq();
            if target == 0 {
                continue;
            }
            shared.set_force_flush(true);
            // `notify_one` keeps a permit for a flush task between two waits.
            shared.flush_notify.notify_one();
            targets.push((table, target));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut done = vec![false; targets.len()];
        loop {
            for (index, (table, target)) in targets.iter().enumerate() {
                if done[index] {
                    continue;
                }
                let table = *table;
                // A failed writer is checked first, so the answer does not depend
                // on whether a flush happened to commit before this poll.
                if self.writer(table).shared().failed() {
                    return Err(FlushNowError::WriterFailed {
                        table: table.name(),
                    });
                }
                if self.committer.flushed_seq(table).await >= *target {
                    self.writer(table).shared().set_force_flush(false);
                    done[index] = true;
                    continue;
                }
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(FlushNowError::ShuttingDown);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(FlushNowError::Timeout {
                        table: table.name(),
                        timeout,
                    });
                }
            }
            if done.iter().all(|finished| *finished) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// One staged-object collection, with an explicit age.
    ///
    /// The background task uses one hour. Tests pass a shorter age. A zero age
    /// collects every row whose `created_at` is before this call.
    #[doc(hidden)]
    pub async fn collect_staged(&self, max_age: std::time::Duration) -> Result<(), error::GcError> {
        gc::collect_once(
            &self.committer,
            &self.objects,
            &self.hooks,
            catalog::now_ns(),
            max_age,
        )
        .await
    }

    /// One obsolete-file collection with this grace. The background task uses
    /// `gc.grace`.
    #[doc(hidden)]
    pub async fn collect_obsolete(
        &self,
        grace: std::time::Duration,
    ) -> Result<ObsoleteReport, error::GcError> {
        gc::collect_obsolete(
            &self.committer,
            &self.objects,
            &self.hooks,
            catalog::now_ns(),
            grace,
        )
        .await
    }

    /// Writes a consistent copy of `catalog.db` to `path` and pins every object it
    /// lists until the returned [`CatalogSnapshot`] is dropped (N M2, D-N-18).
    ///
    /// The gate is taken before the committer lock and held until the snapshot
    /// is dropped, so every object deletion waits for the backup. Commits wait
    /// only for the `VACUUM INTO` itself, which runs under the committer lock;
    /// appends never wait. A failed call removes the partial copy at `path`.
    ///
    /// # Errors
    ///
    /// [`SnapshotError`] when `path` exists or is not UTF-8, or when SQLite fails.
    pub async fn snapshot_catalog(&self, path: &Path) -> Result<CatalogSnapshot, SnapshotError> {
        if path.exists() {
            return Err(SnapshotError::Exists);
        }
        let Some(target) = path.to_str() else {
            return Err(SnapshotError::NonUtf8Path);
        };
        let gate = Arc::clone(&self.committer.object_gc_gate)
            .write_owned()
            .await;
        let copied = match self.committer.snapshot_into(target).await {
            Ok(catalog) => snapshot::read_copy(path)
                .await
                .map(|(objects, flushed_seq)| (catalog, objects, flushed_seq)),
            Err(error) => Err(error),
        };
        let (catalog, objects, flushed_seq) = match copied {
            Ok(copied) => copied,
            Err(error) => {
                let _ = tokio::fs::remove_file(path).await;
                return Err(error);
            }
        };
        Ok(CatalogSnapshot::new(
            path.to_path_buf(),
            objects,
            flushed_seq,
            catalog,
            gate,
        ))
    }

    /// Puts every object of `snapshot` under `dest` at its store-relative key: a
    /// hard link, or a copy when `dest` is on another filesystem (N M2, D-N-18).
    /// Local mode only.
    ///
    /// The copy runs on a blocking thread and keeps going when the caller's future
    /// is dropped. Once `cancel` is set (by the caller, under its lock; see
    /// [`CopyCancel`]), the copy places no further object and returns
    /// [`CopyObjectsError::Cancelled`].
    ///
    /// # Errors
    ///
    /// [`CopyObjectsError::RemoteStore`] in S3 mode; [`CopyObjectsError::Io`]
    /// when an object is missing or cannot be linked or copied;
    /// [`CopyObjectsError::Cancelled`] when `cancel` is set.
    pub async fn copy_objects(
        &self,
        snapshot: &CatalogSnapshot,
        dest: &Path,
        cancel: CopyCancel,
    ) -> Result<CopyReport, CopyObjectsError> {
        let Some(root) = self.objects.local_root() else {
            return Err(CopyObjectsError::RemoteStore);
        };
        let root = root.to_path_buf();
        let dest = dest.to_path_buf();
        let objects = snapshot.objects.clone();
        tokio::task::spawn_blocking(move || snapshot::copy_all(&root, &dest, &objects, &cancel))
            .await
            .map_err(|_| CopyObjectsError::Task)?
    }

    /// One compaction pass at `now` (ns), run to completion. The background
    /// task passes the wall clock every `compaction.interval`.
    #[doc(hidden)]
    pub async fn compact(&self, now: i64) -> Result<CompactionReport, CompactionError> {
        self.compactor.pass(now).await
    }

    /// One deletion pass at `now` (ns), run to completion: rewrites, raises, and
    /// marks applied. The background task runs one after each compaction pass.
    /// After a deletion is applied, its project's unreferenced media is
    /// collected at the same `now` (D-A1-6); a media error changes nothing here.
    #[doc(hidden)]
    pub async fn apply_deletions(&self, now: i64) -> Result<DeletionReport, CompactionError> {
        let report = self.compactor.deletion_pass(now).await?;
        for project in &report.applied_projects {
            if let Err(error) = self.media.collect(*project, now).await {
                // The label only: the error names an object path, which holds
                // the blob's hash (spec 03 §9).
                tracing::warn!(
                    project = %project,
                    error = error.label(),
                    "media collection failed"
                );
            }
        }
        Ok(report)
    }

    /// One media collection over every project at `now` (ns). The background
    /// task runs one daily.
    #[doc(hidden)]
    pub async fn collect_media(&self, now: i64) -> Result<MediaReport, MediaError> {
        self.media.collect_all(now).await
    }

    /// Replaces the compaction policy read at the start of each pass.
    #[cfg(test)]
    pub(crate) fn set_policy(&self, policy: compaction::plan::Policy) {
        self.compactor.set_policy(policy);
    }

    /// Files currently claimed by a compaction job.
    #[cfg(test)]
    pub(crate) fn compaction_claims(&self) -> usize {
        self.compactor.claimed()
    }

    /// The suspect files waiting for the verify pass.
    #[cfg(test)]
    pub(crate) fn verify_queue(&self) -> &VerifyQueue {
        &self.verify
    }

    /// One verify pass at the wall clock, run to completion.
    #[cfg(test)]
    pub(crate) async fn verify_pass(&self) -> compaction::verify_pass::VerifyReport {
        self.compactor.verify_pass(catalog::now_ns()).await
    }

    /// Commits `edit` and publishes the catalog, for tests that race a job.
    #[cfg(test)]
    pub(crate) async fn commit_test_edit(
        &self,
        edit: &catalog::CatalogEdit,
    ) -> Result<Arc<catalog::Catalog>, error::CommitError> {
        let published = Arc::clone(&self.published);
        self.committer
            .commit(edit, move |next| {
                read_state::publish_catalog(&published, next);
            })
            .await
    }

    /// Stops writers, flushes both memtables, closes `catalog.db`, and releases
    /// the data directory lock.
    ///
    /// Other tasks may still hold this value and be inside `append`. An append
    /// that has not been acknowledged returns [`AppendError::ShuttingDown`].
    /// A query must not run after this returns: the lock, and with it the
    /// spill directory, belongs to the next [`Self::open`].
    /// A second call reports the same writer failure as the first, or `Ok`
    /// when the writers already stopped cleanly.
    ///
    /// Each flush task gets up to 30 seconds, together, to commit. A flush that
    /// is still running is cancelled; its rows stay in the WAL and are replayed
    /// on the next [`Self::open`].
    ///
    /// `catalog.db` is closed before the lock is released: closing its
    /// connection checkpoints the SQLite WAL into the file, and that write must
    /// not land after the next owner of the directory has started. A failed
    /// close is reported after the writer failures.
    pub async fn shutdown(&self) -> Result<(), StorageError> {
        self.shutting_down.store(true, Ordering::Release);
        // Compaction stops first, so a job never commits against a catalog
        // another shutdown step is closing. Jobs see `stopping` before their
        // next slice or their commit and give up; staged outputs and temp
        // files are GC's and startup cleanup's problem.
        self.compactor.stop();
        self.compaction
            .stop_and_wait(std::time::Duration::from_secs(30))
            .await;
        self.gc.abort();
        self.disk_monitor.abort();
        let spans = self.spans.shutdown().await;
        let scores = self.scores.shutdown().await;
        self.finish_flushes().await;
        let closed = self.committer.close().await;
        drop(
            self.lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        writer_exit("spans", &spans)?;
        writer_exit("scores", &scores)?;
        closed?;
        Ok(())
    }

    async fn finish_flushes(&self) {
        self.spans_flush.request_stop();
        self.scores_flush.request_stop();
        let spans = self.spans_flush.take_task();
        let scores = self.scores_flush.take_task();
        let spans_abort = spans.as_ref().map(tokio::task::JoinHandle::abort_handle);
        let scores_abort = scores.as_ref().map(tokio::task::JoinHandle::abort_handle);
        let wait = async move {
            if let Some(task) = spans {
                let _ = task.await;
            }
            if let Some(task) = scores {
                let _ = task.await;
            }
        };
        tokio::pin!(wait);
        if tokio::time::timeout(std::time::Duration::from_secs(30), &mut wait)
            .await
            .is_err()
        {
            tracing::warn!(
                "flush did not finish before shutdown; acknowledged rows remain in the WAL"
            );
            if let Some(handle) = spans_abort {
                handle.abort();
            }
            if let Some(handle) = scores_abort {
                handle.abort();
            }
            let _ = wait.await;
        }
    }

    async fn append(
        &self,
        table: Table,
        project: ProjectId,
        batch: Batch,
    ) -> Result<AppendReceipt, AppendError> {
        // Held until `writer.append` returns, so `drop_project`'s write guard
        // waits for every in-flight append and holds new ones while it
        // commits (spec 03 §7).
        let _gate = self.append_gate.read().await;
        let writer = self.writer(table);
        if writer.shared().failed() {
            return Err(AppendError::WalFailed);
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(AppendError::ShuttingDown);
        }
        if self.published.load().projects.is_dropped(project) {
            return Err(AppendError::ProjectDropped);
        }
        if self.disk.low() {
            return Err(AppendError::DiskFull);
        }
        validate_batch(table, &project, &batch)?;
        let bytes = batch.memory_bytes();
        let adding = u64::try_from(bytes).unwrap_or(u64::MAX);
        if writer.shared().memtable_bytes().saturating_add(adding) > self.hard_limit {
            return Err(AppendError::Backpressure);
        }
        writer.append(project, batch, bytes).await
    }

    fn writer(&self, table: Table) -> &WalWriterHandle {
        match table {
            Table::Spans => &self.spans,
            Table::Scores => &self.scores,
        }
    }

    /// Free disk and the reservations of the compaction jobs.
    #[cfg(test)]
    pub(crate) fn disk(&self) -> &disk::Disk {
        &self.disk
    }

    /// Pause and fault points shared with the writers. A no-op outside tests.
    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Rows currently published for `table`.
    #[cfg(test)]
    pub(crate) fn published_rows(&self, table: Table) -> usize {
        let state = self.published.load();
        let snapshot = match table {
            Table::Spans => &state.spans,
            Table::Scores => &state.scores,
        };
        snapshot
            .partitions
            .values()
            .flatten()
            .map(|batch| batch.batch.num_rows())
            .sum()
    }

    /// Active catalog files for `table`, including `traces`.
    #[cfg(test)]
    pub(crate) fn catalog_files(
        &self,
        table: impl Into<catalog::FileTable>,
    ) -> Vec<Arc<catalog::FileMeta>> {
        self.published.load().catalog.test_files(table.into())
    }

    /// Paths of the active files of a file table, including `traces`.
    #[cfg(test)]
    pub(crate) fn catalog_paths(&self, table: catalog::FileTable) -> Vec<String> {
        self.published
            .load()
            .catalog
            .test_files(table)
            .iter()
            .map(|file| file.path.clone())
            .collect()
    }

    /// Rows in the active catalog files of `table`.
    #[cfg(test)]
    pub(crate) fn catalog_rows(&self, table: Table) -> u64 {
        self.catalog_files(table).iter().map(|file| file.rows).sum()
    }

    /// WAL records replayed into the memtable at the last open.
    #[cfg(test)]
    fn replayed(&self, table: Table) -> u64 {
        self.replayed[catalog::table_index(table)]
    }

    /// `flushed_seq` stored for `table`.
    #[cfg(test)]
    async fn flushed_seq(&self, table: Table) -> u64 {
        self.committer.flushed_seq(table).await
    }

    /// Paths left in `staged_objects`.
    #[cfg(test)]
    async fn staged_paths(&self) -> Result<Vec<String>, error::CatalogError> {
        self.committer.staged_paths().await
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.spans.abort();
        self.scores.abort();
        self.spans_flush.abort();
        self.scores_flush.abort();
        self.compactor.stop();
        self.compaction.abort();
        self.gc.abort();
        self.disk_monitor.abort();
    }
}

struct Recovered {
    spans: RecoveredTable,
    scores: RecoveredTable,
    spans_records: u64,
    scores_records: u64,
}

struct RecoveredTable {
    memtable: Memtable,
    segment: SegmentWriter,
    next_seq: u64,
}

fn prepare_layout(data_dir: &Path) -> Result<File, OpenError> {
    ensure_dir(data_dir)?;
    let wal = data_dir.join("wal");
    ensure_dir(&wal)?;
    ensure_dir(&wal.join(Table::Spans.name()))?;
    ensure_dir(&wal.join(Table::Scores.name()))?;
    let tmp = data_dir.join("tmp");
    ensure_dir(&tmp)?;
    ensure_dir(&tmp.join("query"))?;
    lock_dir(data_dir)
}

/// Removes every entry of `spill_dir`: the spill files of a process that
/// crashed. Returns how many were removed; a missing entry is not an error.
fn remove_spill_files(spill_dir: &Path) -> Result<usize, OpenError> {
    let entries = fs::read_dir(spill_dir)
        .map_err(|source| open_io("removing query spill files", spill_dir, source))?;
    let mut removed = 0;
    for entry in entries {
        let entry =
            entry.map_err(|source| open_io("removing query spill files", spill_dir, source))?;
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .map_err(|source| open_io("removing query spill files", &path, source))?
            .is_dir();
        let result = if is_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(open_io("removing query spill files", &path, source)),
        }
    }
    Ok(removed)
}

/// The query memory limits (spec 03 §6): the shared pool's size and each
/// query's admission. The shared size is `query.max_memory_total`, or the
/// machine's default when absent. On a machine smaller than `max_memory`, it
/// and the per-query cap shrink to the machine's memory, but never below the
/// initial bytes.
fn resolve_query_memory(query: &QueryConfig) -> (QueryMemory, usize) {
    let mut total = query
        .max_memory_total
        .map_or_else(memory::default_total_memory, |total| total.0);
    let mut cap = query.max_memory.0;
    if total < cap {
        cap = total.max(query.initial_memory.0);
        total = cap;
        tracing::info!(total, cap, "query memory reduced to the machine's memory");
    }
    tracing::info!(total, "query memory pool");
    let to_usize = |bytes: u64| usize::try_from(bytes).unwrap_or(usize::MAX);
    (
        QueryMemory {
            initial: to_usize(query.initial_memory.0),
            cap: to_usize(cap),
            wait: query.admission_wait,
        },
        to_usize(total),
    )
}

fn recover_tables(
    data_dir: &Path,
    spans_flushed: u64,
    scores_flushed: u64,
    skip_corrupt: bool,
    segment_bytes: u64,
    hooks: &Hooks,
) -> Result<Recovered, OpenError> {
    let wal = data_dir.join("wal");
    let spans = recover_table(
        &wal.join(Table::Spans.name()),
        Table::Spans,
        spans_flushed,
        skip_corrupt,
        segment_bytes,
        hooks,
    )?;
    let scores = recover_table(
        &wal.join(Table::Scores.name()),
        Table::Scores,
        scores_flushed,
        skip_corrupt,
        segment_bytes,
        hooks,
    )?;
    Ok(Recovered {
        spans_records: spans.records,
        scores_records: scores.records,
        spans: spans.table,
        scores: scores.table,
    })
}

struct ReplayTable {
    table: RecoveredTable,
    records: u64,
}

fn recover_table(
    dir: &Path,
    table: Table,
    after_seq: u64,
    skip_corrupt: bool,
    segment_bytes: u64,
    hooks: &Hooks,
) -> Result<ReplayTable, OpenError> {
    let mut memtable = Memtable::new(table);
    let outcome = replay(dir, table, after_seq, skip_corrupt, &mut |seq, batch| {
        replay_batch(&mut memtable, seq, batch)
    })
    .map_err(replay_error)?;
    tracing::info!(
        table = table.name(),
        records = outcome.records,
        truncated_bytes = outcome.truncated_bytes,
        skipped_bytes = outcome.skipped_bytes,
        next_seq = outcome.next_seq,
        memtable_bytes = memtable.bytes(),
        "replayed WAL"
    );
    // The replayed segment is no longer the last once the new one exists, so
    // its unsynced tail must be durable first (spec 03 § 1).
    if let Some(last) = &outcome.last_segment {
        OpenOptions::new()
            .write(true)
            .open(last)
            .and_then(|file| file.sync_all())
            .map_err(|source| OpenError::Io {
                action: "syncing WAL segment",
                path: last.clone(),
                source,
            })?;
    }
    let segment = SegmentWriter::create(dir, outcome.next_seq, segment_bytes, share_hooks(hooks))?;
    Ok(ReplayTable {
        records: outcome.records,
        table: RecoveredTable {
            memtable,
            segment,
            next_seq: outcome.next_seq,
        },
    })
}

/// Inserts one replayed batch into `memtable`, as WAL recovery does. An empty batch is skipped.
fn replay_batch(memtable: &mut Memtable, seq: u64, batch: Batch) -> Result<(), WalError> {
    if batch.num_rows() == 0 {
        return Ok(());
    }
    let project = project_of(&batch)?;
    memtable
        .insert(project, batch, seq)
        .map_err(|err| memtable_to_wal(&err))
}

/// Bytes of the regular files in `dir` (a WAL directory, for [`Storage::stats`]). An
/// unreadable directory or file counts as 0 and is logged without its path.
fn wal_bytes(dir: &Path) -> u64 {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(kind = ?error.kind(), "reading the WAL directory failed");
            return 0;
        }
    };
    let mut total = 0_u64;
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                tracing::debug!(kind = ?error.kind(), "reading a WAL entry failed");
                continue;
            }
        };
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => total = total.saturating_add(meta.len()),
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(kind = ?error.kind(), "reading a WAL file's metadata failed");
            }
        }
    }
    total
}

/// The steps [`Storage::open`] and [`Maintenance::open`] share: validates `config`,
/// takes `LOCK`, migrates `catalog.db` (after its pre-migration copy) and opens the
/// object store. Nothing is replayed, collected or started here.
async fn open_shared(
    config: &StorageConfig,
    hooks: &Hooks,
    remote: Option<(Arc<dyn object_store::ObjectStore>, PutSizes)>,
) -> Result<(File, Committer, Objects), OpenError> {
    config.validate()?;
    let data_dir = config.data_dir.clone();
    let layout_dir = data_dir.clone();
    let lock = tokio::task::spawn_blocking(move || prepare_layout(&layout_dir))
        .await
        .map_err(|_| {
            open_io(
                "opening data directory",
                &data_dir,
                io::Error::other("open task panicked"),
            )
        })??;
    // Refusals come before anything in the directory is written (spec 03
    // § 12): the catalog's marker and migrations, then every WAL segment
    // header, all read-only.
    catalog::check_catalog(&data_dir).await?;
    let wal_dir = data_dir.join("wal");
    for table in [Table::Spans, Table::Scores] {
        wal::check_headers(&wal_dir.join(table.name()), table).map_err(replay_error)?;
    }
    let committer = Committer::open(&data_dir, share_hooks(hooks)).await?;
    let objects = open_objects(config, hooks, remote).await?;
    Ok((lock, committer, objects))
}

async fn open_objects(
    config: &StorageConfig,
    hooks: &Hooks,
    remote: Option<(Arc<dyn object_store::ObjectStore>, PutSizes)>,
) -> Result<Objects, OpenError> {
    #[cfg(test)]
    if let Some((store, put_sizes)) = remote {
        return Ok(Objects::remote_for_test(
            store,
            &config.data_dir,
            share_hooks(hooks),
            put_sizes,
        )?);
    }
    #[cfg(not(test))]
    let _ = remote;
    Ok(Objects::new(config, share_hooks(hooks)).await?)
}

/// Capacity of the S3 read cache (spec 03 §6): `query.read_cache_bytes`, or
/// the default for the file system holding `data_dir`.
fn read_cache_capacity(query: &QueryConfig, data_dir: &Path) -> Result<u64, OpenError> {
    match query.read_cache_bytes {
        Some(bytes) => Ok(bytes.0),
        None => Ok(read_cache::default_capacity(disk::total_bytes(data_dir)?)),
    }
}

struct WriterTune {
    sync: WalSync,
    group_max_bytes: u64,
    group_max_wait: std::time::Duration,
    segment_bytes: u64,
    flush_max_bytes: u64,
}

fn spawn_writer(
    table: Table,
    data_dir: &Path,
    recovered: RecoveredTable,
    tune: &WriterTune,
    published: &Arc<Published>,
    hooks: &Hooks,
) -> WalWriterHandle {
    spawn(WriterParams {
        table,
        dir: data_dir.join("wal").join(table.name()),
        segment: recovered.segment,
        next_seq: recovered.next_seq,
        sync: tune.sync,
        group_max_bytes: tune.group_max_bytes,
        group_max_wait: tune.group_max_wait,
        segment_bytes: tune.segment_bytes,
        flush_max_bytes: tune.flush_max_bytes,
        hooks: share_hooks(hooks),
        published: Arc::clone(published),
        memtable: recovered.memtable,
    })
}

fn project_of(batch: &Batch) -> Result<ProjectId, WalError> {
    let Some(column) = batch.column_by_name(cetra_core::schema::spans::PROJECT_ID) else {
        return Err(WalError::InvalidPayload {
            reason: "batch is missing project_id",
        });
    };
    if column.data_type() != cetra_column::DataType::Utf8 {
        return Err(WalError::InvalidPayload {
            reason: "project_id has an unexpected type",
        });
    }
    if batch.num_rows() == 0 {
        return Err(WalError::InvalidPayload {
            reason: "project_id is null",
        });
    }
    if let cetra_column::Column::Const { value, .. } = column {
        return match value {
            cetra_column::ScalarValue::Utf8(first) => {
                first.parse().map_err(|_| WalError::InvalidPayload {
                    reason: "invalid project id",
                })
            }
            cetra_column::ScalarValue::Null(_) => Err(WalError::InvalidPayload {
                reason: "project_id is null",
            }),
            _ => Err(WalError::InvalidPayload {
                reason: "project_id has an unexpected type",
            }),
        };
    }

    let mut first: Option<&str> = None;
    let mut check_row = |row: usize| -> Result<(), WalError> {
        let Some(val) = read_utf8(column, row) else {
            return Err(WalError::InvalidPayload {
                reason: "project_id is null",
            });
        };
        match first {
            None => first = Some(val),
            Some(expected) if val != expected => {
                return Err(WalError::InvalidPayload {
                    reason: "batch contains more than one project",
                });
            }
            Some(_) => {}
        }
        Ok(())
    };
    match batch.selection() {
        cetra_column::Selection::All => {
            for row in 0..batch.raw_len() {
                check_row(row)?;
            }
        }
        cetra_column::Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                check_row(row)?;
            }
        }
        cetra_column::Selection::Indices(indices) => {
            for &row in indices {
                check_row(row as usize)?;
            }
        }
    }
    let Some(first) = first else {
        return Err(WalError::InvalidPayload {
            reason: "project_id is null",
        });
    };
    first.parse().map_err(|_| WalError::InvalidPayload {
        reason: "invalid project id",
    })
}

fn read_utf8(col: &cetra_column::Column, row: usize) -> Option<&str> {
    match col {
        cetra_column::Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                cetra_column::FlatData::Utf8(u) => u.get(row),
                _ => None,
            }
        }
        cetra_column::Column::Const { value, len } => {
            (row < *len).then_some(value).and_then(|v| match v {
                cetra_column::ScalarValue::Utf8(s) => Some(s.as_str()),
                _ => None,
            })
        }
        cetra_column::Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                cetra_column::FlatData::Utf8(u) => u.get(code),
                _ => None,
            }
        }
        cetra_column::Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                cetra_column::FlatData::Utf8(u) => u.get(run),
                _ => None,
            }
        }
    }
}

fn memtable_to_wal(err: &MemtableError) -> WalError {
    let reason = match err {
        MemtableError::TimeOutOfRange => "time column is outside the supported range",
        MemtableError::MissingColumn { .. } => "batch is missing a required column",
        MemtableError::BadColumn { .. } => "time column has an unexpected type or a null",
        MemtableError::Column(_) => "building a memtable batch",
    };
    WalError::InvalidPayload { reason }
}

fn replay_error(err: WalError) -> OpenError {
    match err {
        WalError::Corrupt {
            table,
            segment,
            offset,
        } => OpenError::WalCorrupt {
            table,
            segment,
            offset,
        },
        other => OpenError::WalReplay(other),
    }
}

fn validate_batch(table: Table, project: &ProjectId, batch: &Batch) -> Result<(), AppendError> {
    if batch.num_rows() == 0 {
        return Err(AppendError::InvalidBatch {
            reason: "batch has no rows",
        });
    }
    let expected_schema = match table {
        Table::Spans => cetra_core::schema::spans_schema(),
        Table::Scores => cetra_core::schema::scores_schema(),
    };
    if batch.schema().as_ref() != expected_schema.as_ref() {
        return Err(AppendError::InvalidBatch {
            reason: "batch schema does not match the table",
        });
    }
    let Some(column) = batch.column_by_name(cetra_core::schema::spans::PROJECT_ID) else {
        return Err(AppendError::InvalidBatch {
            reason: "batch schema does not match the table",
        });
    };
    if column.data_type() != cetra_column::DataType::Utf8 {
        return Err(AppendError::InvalidBatch {
            reason: "batch schema does not match the table",
        });
    }
    let expected = project.to_string();
    let check_row = |row: usize| -> Result<(), AppendError> {
        match read_utf8(column, row) {
            Some(val) if val == expected => Ok(()),
            _ => Err(AppendError::InvalidBatch {
                reason: "batch project_id does not match the project",
            }),
        }
    };
    match batch.selection() {
        cetra_column::Selection::All => {
            for row in 0..batch.raw_len() {
                check_row(row)?;
            }
        }
        cetra_column::Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                check_row(row)?;
            }
        }
        cetra_column::Selection::Indices(indices) => {
            for &row in indices {
                check_row(row as usize)?;
            }
        }
    }
    Ok(())
}

/// Creates `path` if it is missing, then fsyncs its parent. The sync runs
/// whether this call created the directory or found it: a killed process may
/// have created it without the sync.
fn ensure_dir(path: &Path) -> Result<(), OpenError> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(open_io("creating directory", path, source)),
    }
    if let Some(parent) = fs::parent_dir(path) {
        fs::sync_dir(parent).map_err(|source| open_io("syncing directory", parent, source))?;
    }
    Ok(())
}

fn lock_dir(data_dir: &Path) -> Result<File, OpenError> {
    let path = data_dir.join("LOCK");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|source| open_io("opening lock file", &path, source))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(OpenError::Locked {
                path: data_dir.to_path_buf(),
            });
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(open_io("locking data directory", data_dir, source));
        }
    }
    fs::sync_dir(data_dir).map_err(|source| open_io("syncing directory", data_dir, source))?;
    Ok(file)
}

fn open_io(action: &'static str, path: &Path, source: io::Error) -> OpenError {
    OpenError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

fn writer_exit(table: &'static str, exit: &Result<(), WriterExit>) -> Result<(), StorageError> {
    match exit {
        Ok(()) => Ok(()),
        Err(WriterExit::Failed) => Err(StorageError::WalFailed { table }),
        Err(WriterExit::Panicked) => Err(StorageError::WriterPanicked { table }),
    }
}

fn share_hooks(hooks: &Hooks) -> Hooks {
    // Production hooks are `Copy`. Tests share one value and have to clone it.
    #[allow(clippy::clone_on_copy)]
    hooks.clone()
}

/// Whether the memory budgets add up to more than the detected limit. A detected
/// limit of 0 means the OS reported nothing, so there is nothing to compare with.
fn budgets_exceed_memory(budget: u64, detected: u64) -> bool {
    detected > 0 && budget > detected
}

/// Clears both tables' force flags when [`Storage::flush_now`] returns, and
/// also when its future is dropped.
struct ForceFlushGuard<'a>(&'a Storage);

impl Drop for ForceFlushGuard<'_> {
    fn drop(&mut self) {
        self.0.spans.shared().set_force_flush(false);
        self.0.scores.shared().set_force_flush(false);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::sync::Arc;
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_column::{Column, DataType, Field, FlatColumn, FlatData, Schema, Utf8Data};
    use cetra_core::enums::ScoreSource;
    use cetra_core::ids::{ProjectId, TraceId};
    use cetra_core::schema::{spans, spans_schema};
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use object_store::ObjectStoreExt;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Connection};

    use super::*;
    use crate::catalog::FileTable;
    use crate::compaction::job::tests as jobs;
    use crate::config::{FlushConfig, MemtableConfig, QueryConfig, S3Config, WalConfig};
    use crate::error::{CatalogError, StorageError};
    use crate::format::tests_support;
    use crate::hooks::HookPoint;
    use crate::media::MediaBlob;
    use crate::objects::PutSizes;
    use crate::partials::partials_from_spans_batch;
    use crate::reads::columns::{active_rows, i64_at, require_col, string_at};
    use crate::test_support::{Call, ChaosStore, parquet_rows, score_rows, span_row, span_rows};
    use crate::views::QueryContext;
    use crate::wal::segment::SEGMENT_HEADER_LEN;
    use crate::wal::{parse_segment_name, segment_name, spans_at, spans_keyed, spans_named};

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn other_project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap()
    }

    fn config_in(dir: &Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig::default(),
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        }
    }

    /// The free-disk fields of `health` follow the measurement (D-N-31).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn health_reports_low_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.min_free_bytes = ByteSize::b(100);
        let storage = Storage::open(config).await.unwrap();

        storage.disk().set_free_for_test(Some(0));
        let low = storage.health();
        assert!(low.disk_low);
        assert_eq!(low.disk_free_bytes, 0);
        assert_eq!(low.disk_min_free_bytes, 100);

        storage.disk().set_free_for_test(Some(u64::MAX));
        let high = storage.health();
        assert!(!high.disk_low);
        assert_eq!(high.disk_free_bytes, u64::MAX);
        storage.shutdown().await.unwrap();
    }

    /// The first probe writes the marker in local mode; the next one finds it (D-N-36).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn probe_objects_writes_the_marker_once() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();

        storage.probe_objects().await.unwrap();
        assert!(dir.path().join("objects/_probe/readyz").is_file());
        storage.probe_objects().await.unwrap();
        storage.shutdown().await.unwrap();
    }

    /// A store that refuses every HEAD and PUT makes the probe `Unreachable` (D-N-36).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn probe_objects_fails_when_the_store_fails() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let chaos = ChaosStore::new(inner);
        let storage =
            Storage::open_with_store(config_in(dir.path()), Arc::new(chaos.clone()), PutSizes::S3)
                .await
                .unwrap();

        chaos.hold(&[Call::Head, Call::Put]);
        assert!(matches!(
            storage.probe_objects().await,
            Err(ProbeError::Unreachable)
        ));
        chaos.release();
        storage.shutdown().await.unwrap();
    }

    /// A HEAD refused for want of `s3:ListBucket` (403) still passes: the PUT answers (D-N-36).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn probe_objects_writes_when_head_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let chaos = ChaosStore::new(inner);
        let storage =
            Storage::open_with_store(config_in(dir.path()), Arc::new(chaos.clone()), PutSizes::S3)
                .await
                .unwrap();

        chaos.hold(&[Call::Head]);
        storage.probe_objects().await.unwrap();
        chaos.release();
        storage.shutdown().await.unwrap();
    }

    /// A committed flush counts, with its time, and no failed flush is counted (D-N-31).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stats_count_a_flush() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        // The flush task bumps the counters just after its commit returns.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while storage.stats().await.flushes_ok[0] == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let stats = storage.stats().await;
        assert!(stats.flushes_ok[0] >= 1, "{stats:?}");
        assert!(stats.flush_nanos[0] > 0, "{stats:?}");
        assert_eq!(stats.flushes_failed[0], 0, "{stats:?}");
        storage.shutdown().await.unwrap();
    }

    /// The spans WAL has bytes on disk after an append, before any flush (D-N-31).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stats_report_wal_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();

        assert!(storage.stats().await.wal_bytes[0] > 0);
        storage.shutdown().await.unwrap();
    }

    /// `delete_objects` is not a TOML key: a file cannot turn object deletion off (D-N-34).
    #[test]
    fn delete_objects_is_not_a_toml_key() {
        let base = "data_dir = \"data\"\n";
        let config: StorageConfig = toml::from_str(base).unwrap();
        assert!(config.delete_objects);
        assert!(
            toml::from_str::<StorageConfig>(&format!("{base}delete_objects = false\n")).is_err()
        );
    }

    /// Three distinct traces spread over two UTC days (one partition per day).
    fn three_traces_two_days() -> Batch {
        const DAY_NS: i64 = 86_400_000_000_000;
        let rows = [
            span_row(1, 0, 1_000, 1),
            span_row(2, 0, 1_100, 2),
            span_row(3, 0, DAY_NS + 2_000, 3),
        ];
        span_rows(&rows)
    }

    /// Reads one Parquet object back with the sync reader.
    fn read_parquet(dir: &Path, path: &str) -> Batch {
        let bytes = std::fs::read(dir.join("objects").join(path)).unwrap();
        parquet_rows(bytes::Bytes::from(bytes))
    }

    /// Compares two batches column by column.
    fn assert_same_rows(actual: &Batch, expected: &Batch, label: &str) {
        assert_eq!(actual.num_rows(), expected.num_rows(), "{label}: rows");
        for field in expected.schema().fields() {
            let name = field.name.as_str();
            assert_eq!(
                actual.column_by_name(name),
                expected.column_by_name(name),
                "{label}: {name}"
            );
        }
    }

    /// `(spans path, traces path)` for every `file_sources` row.
    async fn source_pairs(dir: &Path) -> Vec<(String, String)> {
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let rows = sqlx::query(
            "SELECT s.path AS spans_path, t.path AS traces_path
             FROM file_sources fs
             JOIN data_files s ON s.id = fs.spans_file_id
             JOIN data_files t ON t.id = fs.traces_file_id",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap();
        rows.iter()
            .map(|row| {
                (
                    sqlx::Row::try_get(row, "spans_path").unwrap(),
                    sqlx::Row::try_get(row, "traces_path").unwrap(),
                )
            })
            .collect()
    }

    fn score_batch(name: &str) -> Batch {
        let score = Score {
            project_id: project(),
            score_id: "018f6b4e-7c1a-7c1a-8c1a-0000000000ab".parse().unwrap(),
            target: ScoreTarget::Trace {
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736"
                    .parse::<TraceId>()
                    .unwrap(),
            },
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value: ScoreValue::Numeric(0.5),
            reason: None,
            source: ScoreSource::Api,
            evaluator_id: None,
            rule_id: None,
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: std::collections::BTreeMap::new(),
            created_at: 5,
            updated_at: 6,
            deleted: false,
            ingest_time: 7,
        };
        score_rows(&[score])
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reopen_after_drop_continues_sequence_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let span = storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let score = storage
            .append_scores(project(), score_batch("accuracy"))
            .await
            .unwrap();
        assert_eq!(span.seq, 1);
        assert_eq!(score.seq, 1);
        drop(storage);
        tokio::task::yield_now().await;
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.published_rows(Table::Spans), 1);
        assert_eq!(storage.published_rows(Table::Scores), 1);
        let span = storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();
        let score = storage
            .append_scores(project(), score_batch("accuracy"))
            .await
            .unwrap();
        assert_eq!((span.seq, score.seq), (2, 2));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_open_is_locked_until_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let first = Storage::open(config.clone()).await.unwrap();
        let error = Storage::open(config.clone()).await.unwrap_err();
        assert!(matches!(error, OpenError::Locked { .. }), "{error}");
        first.shutdown().await.unwrap();
        let second = Storage::open(config).await.unwrap();
        second.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn invalid_batches_use_static_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let marker = "marker-7f3a9c2e-not-a-real-row";

        let empty = Batch::empty(spans_schema());
        let error = storage.append_spans(project(), empty).await.unwrap_err();
        assert_eq!(
            error,
            AppendError::InvalidBatch {
                reason: "batch has no rows",
            }
        );
        assert!(!error.to_string().contains(marker));

        let schema = Schema::try_new_arc(vec![Field::new(marker, DataType::Utf8, true)]).unwrap();
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([marker]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let wrong_schema = Batch::try_new_all(schema, vec![col], 1).unwrap();
        let error = storage
            .append_spans(project(), wrong_schema)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AppendError::InvalidBatch {
                reason: "batch schema does not match the table",
            }
        );
        assert!(!error.to_string().contains(marker));

        let wrong_project = spans_named(&[1], marker);
        let error = storage
            .append_spans(other_project(), wrong_project)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            AppendError::InvalidBatch {
                reason: "batch project_id does not match the project",
            }
        );
        assert!(!error.to_string().contains(marker));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backpressure_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.memtable.hard_limit = ByteSize::mib(16);
        config.flush.max_bytes = ByteSize::mib(1);
        let storage = Storage::open(config.clone()).await.unwrap();
        let huge = spans_named(&[1], &"m".repeat(17 * 1024 * 1024));
        let error = storage.append_spans(project(), huge).await.unwrap_err();
        assert_eq!(error, AppendError::Backpressure);
        assert_eq!(storage.published_rows(Table::Spans), 0);
        storage.shutdown().await.unwrap();
        let storage = Storage::open(config).await.unwrap();
        let receipt = storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();
        assert_eq!(receipt.seq, 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_wal_write_error_fails_the_writer_and_replays_acked_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.hooks().arm_fail(HookPoint::WalWrite);
        let error = storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap_err();
        assert_eq!(error, AppendError::WalFailed);
        assert!(storage.health().spans_wal_failed);
        assert!(!storage.health().scores_wal_failed);
        let shutdown = storage.shutdown().await.unwrap_err();
        assert!(matches!(
            shutdown,
            StorageError::WalFailed { table: "spans" }
        ));
        let storage = Storage::open(config).await.unwrap();
        // Shutdown flushed the acknowledged row, so replay has nothing to insert.
        assert_eq!(storage.replayed(Table::Spans), 0);
        assert_eq!(storage.published_rows(Table::Spans), 0);
        assert_eq!(storage.catalog_rows(Table::Spans), 1);
        let receipt = storage
            .append_spans(project(), spans_at(&[3]))
            .await
            .unwrap();
        assert_eq!(receipt.seq, 2);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_torn_tail_is_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();
        // Drop aborts the flush task. Shutdown would commit the rows and the
        // following corruption would no longer be in the replayed WAL.
        drop(storage);
        let segment = span_segments(dir.path())
            .into_iter()
            .find(|path| path.metadata().unwrap().len() > SEGMENT_HEADER_LEN as u64)
            .unwrap();
        let len = segment.metadata().unwrap().len();
        let file = OpenOptions::new().write(true).open(&segment).unwrap();
        file.set_len(len - 1).unwrap();
        drop(file);
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.published_rows(Table::Spans), 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_corrupt_middle_segment_fails_open_unless_skip_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.wal.segment_bytes = ByteSize::mib(1);
        let storage = Storage::open(config.clone()).await.unwrap();
        // Repeated bytes compress under LZ4 and never fill a segment.
        let big = incompressible(2 * 1024 * 1024);
        storage
            .append_spans(project(), spans_named(&[1], &big))
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_named(&[2], &big))
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[3]))
            .await
            .unwrap();
        // Drop leaves the WAL unflushed so the corrupt middle segment is still there.
        drop(storage);

        let mut segments = span_segments(dir.path());
        segments.retain(|path| path.metadata().unwrap().len() > SEGMENT_HEADER_LEN as u64);
        segments.sort();
        assert!(segments.len() >= 3, "{segments:?}");
        let middle = &segments[1];
        let mut bytes = std::fs::read(middle).unwrap();
        bytes[30] ^= 0xff;
        std::fs::write(middle, &bytes).unwrap();

        let error = Storage::open(config.clone()).await.unwrap_err();
        assert!(
            matches!(error, OpenError::WalCorrupt { table: "spans", .. }),
            "{error}"
        );

        config.wal.skip_corrupt = true;
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.published_rows(Table::Spans), 2);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_and_health_do_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(config_in(dir.path())).await.unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let health = {
            let storage = Arc::clone(&storage);
            let done = Arc::clone(&done);
            tokio::spawn(async move {
                while !done.load(Ordering::Relaxed) {
                    let _ = storage.health();
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut tasks = Vec::new();
        for start in 0..32 {
            let storage = Arc::clone(&storage);
            tasks.push(tokio::spawn(async move {
                storage
                    .append_spans(project(), spans_at(&[start]))
                    .await
                    .unwrap();
            }));
        }
        let finished = tokio::time::timeout(Duration::from_secs(15), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await;
        done.store(true, Ordering::Relaxed);
        health.await.unwrap();
        assert!(finished.is_ok(), "append and health deadlocked");
        let storage = Arc::try_unwrap(storage).unwrap();
        assert_eq!(storage.published_rows(Table::Spans), 32);
        storage.shutdown().await.unwrap();
    }

    /// The `span_id`s of every stored span, as bytes.
    async fn span_ids(storage: &Storage) -> Vec<Vec<u8>> {
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let batches = query.collect_raw_spans().await.unwrap();
        let mut ids = Vec::new();
        for batch in &batches {
            let idx = batch.schema().index_of(spans::SPAN_ID).unwrap();
            let col = batch.columns()[idx].as_ref();
            for phys in active_rows(batch) {
                if let Column::Flat(flat) = col
                    && let FlatData::FixedBytes8(arr) = flat.data()
                {
                    ids.push(arr[phys].to_vec());
                } else {
                    panic!("unexpected span_id encoding");
                }
            }
        }
        ids
    }

    async fn span_names(query: ProjectQuery) -> Vec<String> {
        let batches = query.collect_raw_spans().await.unwrap();
        let mut names = Vec::new();
        for batch in &batches {
            let col = require_col(batch, spans::NAME).unwrap();
            for row in 0..batch.num_rows() {
                if let Some(name) = string_at(batch, col, row).unwrap() {
                    names.push(name);
                }
            }
        }
        names
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_query_during_append_stays_on_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(config_in(dir.path())).await.unwrap());
        storage.hooks().hold(HookPoint::AfterWalSync);
        let append = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.append_spans(project(), spans_at(&[1])).await })
        };
        let started = std::time::Instant::now();
        while storage.hooks().waiting_at(HookPoint::AfterWalSync) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "writer never reached AfterWalSync"
            );
            tokio::task::yield_now().await;
        }
        let during = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        assert!(span_names(during.clone()).await.is_empty());
        storage.hooks().release(HookPoint::AfterWalSync);
        append.await.unwrap().unwrap();
        let after = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        assert_eq!(span_names(after).await.len(), 1);
        assert!(span_names(during).await.is_empty());
        storage.shutdown().await.unwrap();
    }

    /// Config whose query memory is `mib` MiB for the shared pool, the per-query
    /// cap and the initial bytes, with a short admission wait.
    fn config_with_query_memory(dir: &Path, mib: u64) -> StorageConfig {
        let mut config = config_in(dir);
        config.query.max_memory_total = Some(ByteSize::mib(mib));
        config.query.max_memory = ByteSize::mib(mib);
        config.query.initial_memory = ByteSize::mib(mib);
        config.query.admission_wait = Duration::from_millis(100);
        config
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn appends_fail_while_the_disk_is_low() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.min_free_bytes = ByteSize::b(1000);
        let storage = Storage::open(config).await.unwrap();
        let bytes = bytes::Bytes::from_static(b"media bytes");
        let blob = MediaBlob {
            sha256: Sha256::digest(&bytes).into(),
            mime_type: "text/plain".to_owned(),
            bytes,
            newest_span_start: 0,
        };

        storage.disk().set_free_for_test(Some(10));
        assert_eq!(
            storage
                .append_spans(project(), spans_at(&[1]))
                .await
                .unwrap_err(),
            AppendError::DiskFull
        );
        assert_eq!(
            storage
                .append_scores(project(), score_batch("accuracy"))
                .await
                .unwrap_err(),
            AppendError::DiskFull
        );
        assert!(matches!(
            storage.put_media(project(), blob.clone()).await,
            Err(MediaError::DiskFull)
        ));

        storage.disk().set_free_for_test(Some(10_000));
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage
            .append_scores(project(), score_batch("accuracy"))
            .await
            .unwrap();
        storage.put_media(project(), blob).await.unwrap();
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_compaction_job_that_does_not_fit_is_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let config = jobs::config_in(
            dir.path(),
            jobs::quiet_compaction(2),
            Duration::from_secs(3_600),
        );
        let mut storage = Storage::open(config.clone()).await.unwrap();
        for index in 0..2_u8 {
            let base = jobs::at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
            let batch = span_rows(&[span_row(0, 10 + index, base, base + 1_000_000_000)]);
            storage = jobs::flush_cycle(&config, storage, batch).await;
        }
        let before = storage.catalog_paths(FileTable::Spans);
        assert_eq!(before.len(), 2);

        storage.disk().set_free_for_test(Some(0));
        let report = storage.compact(jobs::at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (0, 1), "{report:?}");
        assert_eq!(storage.catalog_paths(FileTable::Spans), before);

        storage.disk().set_free_for_test(None);
        let report = storage.compact(jobs::at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
        assert_eq!(storage.catalog_files(Table::Spans).len(), 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_query_waits_then_fails_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_with_query_memory(dir.path(), 2))
            .await
            .unwrap();
        let held = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let busy = storage
            .query(project(), QueryOptions::new(None, None))
            .await;
        assert!(matches!(busy, Err(QueryError::Busy)), "a second query fits");
        drop(held);
        storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_query_that_cannot_spill_fails_as_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_query_memory(dir.path(), 1);
        let storage = Storage::open(config.clone()).await.unwrap();
        // One trace of many spans with long names: the join's build side holds
        // all of them, and a hash join does not spill.
        let spans: Vec<_> = (1..=20_000_u64)
            .map(|index| {
                let mut span = span_row(1, 0, i64::try_from(index).unwrap(), 1);
                span.span_id = cetra_core::ids::SpanId::from_bytes(index.to_be_bytes()).unwrap();
                span.name = "n".repeat(200);
                span
            })
            .collect();
        storage
            .append_spans(project(), span_rows(&spans))
            .await
            .unwrap();
        // Shutdown flushes the memtable to a Parquet file.
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        let ctx = QueryContext::new(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
            None,
        )
        .unwrap();
        let params = serde_json::Map::new();
        let result = ctx
            .sql_json(SqlRequest {
                query: "SELECT a.name AS a_name, b.name AS b_name FROM spans a JOIN spans b ON a.trace_id = b.trace_id LIMIT 1",
                params: &params,
                limit: 1,
                timeout: Duration::from_secs(30),
            })
            .await;
        match result {
            Err(RunError::TooLarge) => {}
            Err(other) => panic!("expected too large, got {other:?}"),
            Ok(_) => panic!("the join fit in one megabyte"),
        }
        drop(ctx);
        storage.shutdown().await.unwrap();
    }

    /// The prompt columns survive a flush: spans keep them and the trace merges
    /// them into the distinct list `prompts` (D-V-15).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prompt_columns_round_trip() {
        use cetra_query::encode::encode_json_result;

        use crate::reads::{
            ListScope, SortOrder, TraceListRequest, TraceSort, trace_detail, trace_list,
        };

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let spans: Vec<_> = [1_u8, 2]
            .into_iter()
            .map(|version| {
                let mut span = span_row(1, version, 1_000 + i64::from(version), 1_000);
                span.prompt_name = Some("p".to_owned());
                span.prompt_version = Some(i64::from(version));
                span
            })
            .collect();
        let trace_id = spans[0].trace_id.to_string();
        storage
            .append_spans(project(), span_rows(&spans))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();

        let ctx = QueryContext::new(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
            None,
        )
        .unwrap();
        let timeout = Duration::from_secs(30);
        let detail = trace_detail(&ctx, &trace_id, timeout)
            .await
            .unwrap()
            .expect("the trace exists");
        let mut span_values = Vec::new();
        for batch in &detail.spans {
            let names_col = require_col(batch, spans::PROMPT_NAME).unwrap();
            let versions_col = require_col(batch, spans::PROMPT_VERSION).unwrap();
            for row in 0..batch.num_rows() {
                span_values.push((
                    string_at(batch, names_col, row).unwrap().unwrap(),
                    i64_at(batch, versions_col, row).unwrap(),
                ));
            }
        }
        assert_eq!(span_values, [("p".to_owned(), 1), ("p".to_owned(), 2)]);

        let list = trace_list(
            &ctx,
            TraceListRequest {
                filter: None,
                search: None,
                strict_exclusions: false,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: 10,
                cursor: None,
                timeout,
            },
        )
        .await
        .unwrap();
        let trace_batch = &list.batches[0];
        let encoded = encode_json_result(
            trace_batch.schema(),
            std::slice::from_ref(trace_batch),
            10,
            SQL_MAX_RESULT_BYTES,
            false,
            None,
        )
        .unwrap();
        let prompts: Vec<String> = encoded.rows[0][cetra_core::schema::traces::PROMPTS]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(prompts, ["p@1", "p@2"]);

        for (filter, matches) in [("prompts @> ['p@2']", 1), ("prompts @> ['p@3']", 0)] {
            let page = trace_list(
                &ctx,
                TraceListRequest {
                    filter: Some(filter),
                    search: None,
                    strict_exclusions: false,
                    scope: ListScope {
                        window: None,
                        include_eval: false,
                        include_experiments: false,
                    },
                    sort: TraceSort::StartTime,
                    order: SortOrder::Desc,
                    limit: 10,
                    cursor: None,
                    timeout,
                },
            )
            .await
            .unwrap();
            let rows: usize = page.batches.iter().map(|b| b.num_rows()).sum();
            assert_eq!(rows, matches, "{filter}");
        }
        drop(ctx);
        storage.shutdown().await.unwrap();
    }

    /// One sort of every span row, whose spill passes through the query's disk manager.
    /// The window sorts every row and the outer count returns one row: there is no top-k
    /// that would avoid the sort.
    async fn spill_query(storage: &Storage) -> Result<(), RunError> {
        let params = serde_json::Map::new();
        let ctx = QueryContext::new(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
            None,
        )
        .unwrap();
        ctx.sql_json(SqlRequest {
            query: "SELECT count(*) AS n FROM (SELECT row_number() OVER (PARTITION BY trace_id ORDER BY input, span_id) AS r FROM spans) WHERE r > 0",
            params: &params,
            limit: 1,
            timeout: Duration::from_secs(30),
        })
        .await
        .map(drop)
    }

    /// Opens storage holding 40,000 spans across 40 traces in a Parquet file, with a
    /// 1 MiB floor. `spill_query` over them sorts all of them globally, which spills under 32 MiB
    /// while keeping each window partition (1,000 spans / ~1 MiB) within the pool.
    async fn spilling_storage(dir: &Path) -> Storage {
        let mut config = config_with_query_memory(dir, 32);
        config.query.initial_memory = ByteSize::mib(16);
        config.query.target_partitions = 1;
        config.min_free_bytes = ByteSize::mib(1);
        let storage = Storage::open(config.clone()).await.unwrap();
        // 40,000 spans across 40 traces (1,000 spans per trace), each with a 1 KiB input.
        let spans: Vec<_> = (1..=40_000_u64)
            .map(|index| {
                let trace = u16::try_from((index - 1) / 1_000).unwrap() + 1;
                let mut span = span_row(trace, 0, i64::try_from(index).unwrap(), 1);
                span.span_id = cetra_core::ids::SpanId::from_bytes(index.to_be_bytes()).unwrap();
                span.input = Some("i".repeat(1024));
                span
            })
            .collect();
        storage
            .append_spans(project(), span_rows(&spans))
            .await
            .unwrap();
        // Shutdown flushes the memtable to a Parquet file.
        storage.shutdown().await.unwrap();
        Storage::open(config).await.unwrap()
    }

    /// Waits until the spill files and the shared query memory are back to 0.
    async fn wait_for_spill_release(storage: &Storage) {
        let started = std::time::Instant::now();
        while storage.qe_runtime.spill.spilled_bytes() != 0
            || storage.qe_runtime.admission.used() != 0
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "spill bytes left: {}, pool bytes left: {}",
                storage.qe_runtime.spill.spilled_bytes(),
                storage.qe_runtime.admission.used()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_spill_failure_leaves_no_reservation_behind() {
        let dir = tempfile::tempdir().unwrap();
        let storage = spilling_storage(dir.path()).await;

        // The free disk is at the threshold, so the spill limit is 0 and the first spill
        // write fails.
        storage.disk().set_free_for_test(Some(1 << 20));
        let error = spill_query(&storage)
            .await
            .expect_err("the spill is past the cap");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
        wait_for_spill_release(&storage).await;
        assert_eq!(storage.qe_runtime.spill.spilled_bytes(), 0);
        assert_eq!(
            storage.qe_runtime.admission.used(),
            0,
            "the failed query's pool bytes"
        );

        // A small query that does not spill succeeds once the disk has room again.
        storage.disk().set_free_for_test(None);
        let params = serde_json::Map::new();
        let ctx = QueryContext::new(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
            None,
        )
        .unwrap();
        ctx.sql_json(SqlRequest {
            query: "SELECT count(*) AS n FROM spans",
            params: &params,
            limit: 1,
            timeout: Duration::from_secs(30),
        })
        .await
        .expect("a small query after the failed spill succeeds");
        drop(ctx);
        wait_for_spill_release(&storage).await;
        storage.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_spill_past_the_cap_fails_as_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let storage = spilling_storage(dir.path()).await;

        // Run 1, on the real free disk: the spill fits.
        spill_query(&storage)
            .await
            .expect("the spill fits on the real disk");

        // Run 2: the free disk is at the threshold, so the spill limit is 0.
        storage.disk().set_free_for_test(Some(1 << 20));
        let error = spill_query(&storage)
            .await
            .expect_err("the spill is past the cap");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
        // The failed query's spill files go away as its tasks wind down.
        let started = std::time::Instant::now();
        while storage.qe_runtime.spill.spilled_bytes() != 0 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "spill files left: {} bytes",
                storage.qe_runtime.spill.spilled_bytes()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Run 3: the threshold is gone, so the spill fits again.
        storage.disk().set_free_for_test(None);
        spill_query(&storage).await.expect("the spill fits again");
        storage.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_removes_leftover_spill_directories() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("tmp").join("query").join("spill-old");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("x"), b"spill").unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        assert!(!stale.exists(), "the leftover spill directory is removed");
        let entries: Vec<String> = std::fs::read_dir(dir.path().join("tmp").join("query"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.is_empty(), "{entries:?}");
        storage.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn open_refuses_a_newer_directory_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("wal").join("spans");
        std::fs::create_dir_all(&wal).unwrap();
        std::fs::write(wal.join("00000000000000000001.wal"), b"a newer segment").unwrap();
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.path().join("catalog.db"))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let before = tests_support::dir_digest(dir.path());

        let Err(error) = Storage::open(config_in(dir.path())).await else {
            panic!("Storage::open should refuse a newer directory");
        };
        assert!(matches!(
            error,
            OpenError::Catalog(CatalogError::FormatTooNew {
                found: 2,
                supported: 1
            })
        ));
        assert_eq!(tests_support::dir_digest(dir.path()), before);

        let Err(error) = Maintenance::open(config_in(dir.path())).await else {
            panic!("Maintenance::open should refuse a newer directory");
        };
        assert!(matches!(
            error,
            OpenError::Catalog(CatalogError::FormatTooNew {
                found: 2,
                supported: 1
            })
        ));
        assert_eq!(tests_support::dir_digest(dir.path()), before);
        let copies = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".pre-"))
            .count();
        assert_eq!(copies, 0);
    }

    /// A segment this build cannot read refuses the open and is left as it is,
    /// whether it has a newer format or no header at all (spec 03 § 1, § 12),
    /// in either table. The other table's WAL holds unflushed records, so a
    /// replay that ran before the refusal would change it.
    #[test]
    fn open_refuses_a_segment_it_cannot_read_and_leaves_it_alone() {
        let mut newer = b"LWAL".to_vec();
        newer.extend_from_slice(&2_u16.to_le_bytes());
        newer.extend_from_slice(&0_u16.to_le_bytes());
        let col = spans_at(&[2]);
        wal::encode(9, &col, &mut newer).unwrap();
        let mut headerless = Vec::new();
        wal::push_framed(&mut headerless, 9, b"not ipc");
        let cases = [
            (
                newer,
                error::HeaderProblem::NewerFormat {
                    found: 2,
                    supported: 1,
                },
            ),
            (headerless, error::HeaderProblem::NotAWalSegment),
        ];
        for (bytes, expected) in cases {
            for bad in [Table::Spans, Table::Scores] {
                let dir = tempfile::tempdir().unwrap();
                let config = config_in(dir.path());
                // Unflushed records in both tables: a crash after the appends.
                test_support::crashed_life(&config, async |storage: &Storage| {
                    storage
                        .append_spans(project(), spans_at(&[1]))
                        .await
                        .unwrap();
                    storage
                        .append_scores(
                            project(),
                            score_rows(&[test_support::score_row(1, 1, 1, 1, 1, false, "q")]),
                        )
                        .await
                        .unwrap();
                });
                settle_catalog(dir.path());
                let wal = dir.path().join("wal").join(bad.name());
                std::fs::write(wal.join(segment_name(9)), &bytes).unwrap();
                let before = tests_support::dir_digest(dir.path());

                let error = refused_open(&config);
                match error {
                    OpenError::WalReplay(WalError::Header { problem, table, .. }) => {
                        assert_eq!(problem, expected);
                        assert_eq!(table, bad.name());
                    }
                    other => panic!("expected a header refusal, got {other}"),
                }
                assert_eq!(
                    tests_support::dir_digest(dir.path()),
                    before,
                    "a refused open changed the directory ({})",
                    bad.name()
                );
            }
        }
    }

    /// A directory from before format versions whose catalog holds no data but
    /// whose WAL is headerless is refused before the catalog is stamped.
    #[test]
    fn a_pre_format_wal_is_refused_before_the_catalog_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        test_support::clean_life(&config, async |_: &Storage| {});
        settle_catalog(dir.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut connection = SqliteConnectOptions::new()
                .filename(dir.path().join("catalog.db"))
                .connect()
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version = 0")
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
        });
        let mut headerless = Vec::new();
        wal::push_framed(&mut headerless, 9, b"not ipc");
        std::fs::write(
            dir.path().join("wal").join("spans").join(segment_name(9)),
            &headerless,
        )
        .unwrap();
        let before = tests_support::dir_digest(dir.path());

        let error = refused_open(&config);
        assert!(
            matches!(
                error,
                OpenError::WalReplay(WalError::Header {
                    problem: error::HeaderProblem::NotAWalSegment,
                    ..
                })
            ),
            "expected NotAWalSegment, got {error}"
        );
        assert_eq!(tests_support::dir_digest(dir.path()), before);
        let marker: i64 = runtime.block_on(async {
            let mut connection = SqliteConnectOptions::new()
                .filename(dir.path().join("catalog.db"))
                .read_only(true)
                .connect()
                .await
                .unwrap();
            let marker = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
            marker
        });
        assert_eq!(marker, 0, "the refused open stamped the catalog");
    }

    /// Opens `config` on a runtime of its own and returns the refusal.
    fn refused_open(config: &StorageConfig) -> OpenError {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        match runtime.block_on(Storage::open(config.clone())) {
            Ok(_) => panic!("Storage::open should refuse the directory"),
            Err(error) => error,
        }
    }

    /// Waits for the previous life's catalog connection to close (sqlx closes
    /// it on a worker thread), then checkpoints what is left, so the digest
    /// taken next sees a settled `catalog.db`.
    fn settle_catalog(dir: &Path) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut connection = SqliteConnectOptions::new()
                .filename(dir.join("catalog.db"))
                .connect()
                .await
                .unwrap();
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
        });
    }

    #[test]
    fn the_budget_warning_needs_a_detected_limit_that_is_exceeded() {
        // A detected limit of 0 means the OS reported nothing: no warning.
        assert!(!budgets_exceed_memory(1 << 30, 0));
        assert!(!budgets_exceed_memory(0, 0));
        // Budgets within the limit, and exactly at it, do not warn.
        assert!(!budgets_exceed_memory(1 << 30, 2 << 30));
        assert!(!budgets_exceed_memory(2 << 30, 2 << 30));
        // Budgets past a real limit warn.
        assert!(budgets_exceed_memory((2 << 30) + 1, 2 << 30));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_sync_is_never_visible() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(config_in(dir.path())).await.unwrap());
        storage
            .append_spans(project(), spans_named(&[1], "kept"))
            .await
            .unwrap();
        storage.hooks().arm_fail(HookPoint::WalSync);
        let append = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move {
                storage
                    .append_spans(project(), spans_named(&[2], "lost"))
                    .await
            })
        };
        while !append.is_finished() {
            let names = span_names(
                storage
                    .query(project(), QueryOptions::new(None, None))
                    .await
                    .unwrap(),
            )
            .await;
            assert!(!names.iter().any(|name| name == "lost"));
            tokio::task::yield_now().await;
        }
        assert_eq!(append.await.unwrap().unwrap_err(), AppendError::WalFailed);
        let names = span_names(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(names, vec!["kept".to_owned()]);
        let shutdown = storage.shutdown().await.unwrap_err();
        assert!(matches!(
            shutdown,
            StorageError::WalFailed { table: "spans" }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_during_appends_keeps_only_acknowledged_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Arc::new(Storage::open(config.clone()).await.unwrap());
        let mut tasks = Vec::new();
        for index in 0..8_u8 {
            let storage = Arc::clone(&storage);
            tasks.push(tokio::spawn(async move {
                // A distinct span id per append: a flush keeps one row per
                // `(trace_id, span_id)`, so equal ids would merge the rows.
                let batch = spans_keyed(&[i64::from(index)], &format!("s{index}"), index + 1);
                storage.append_spans(project(), batch).await
            }));
        }
        tokio::task::yield_now().await;
        storage.shutdown().await.unwrap();
        let mut acknowledged = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => acknowledged += 1,
                Err(AppendError::ShuttingDown) => {}
                Err(error) => panic!("{error}"),
            }
        }
        drop(storage);
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.replayed(Table::Spans), 0);
        assert_eq!(storage.published_rows(Table::Spans), 0);
        assert_eq!(
            storage.catalog_rows(Table::Spans),
            u64::try_from(acknowledged).unwrap_or(u64::MAX)
        );
        storage.shutdown().await.unwrap();
    }

    async fn wait_for_catalog(storage: &Storage, table: Table, rows: u64) {
        let started = std::time::Instant::now();
        loop {
            if storage.catalog_rows(table) == rows && storage.published_rows(table) == 0 {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "flush did not commit {rows} rows"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_aged_memtable_is_flushed_and_skipped_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        let storage = Storage::open(config.clone()).await.unwrap();
        let first = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let second = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        // Each query gets an admitted memory pool of its own (spec 03 §6).
        assert!(!Arc::ptr_eq(
            first.engine().pool().unwrap(),
            second.engine().pool().unwrap()
        ));
        let receipt = storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let started = std::time::Instant::now();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(storage.flushed_seq(Table::Spans).await, receipt.seq);
        let files = storage.catalog_files(Table::Spans);
        assert_eq!(files.len(), 1);
        assert!(!files[0].fully_merged);
        assert_eq!(files[0].level, 0);
        let object = dir.path().join("objects").join(&files[0].path);
        assert_eq!(object.metadata().unwrap().len(), files[0].size);
        assert!(storage.staged_paths().await.unwrap().is_empty());
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.replayed(Table::Spans), 0);
        assert_eq!(storage.published_rows(Table::Spans), 0);
        assert_eq!(storage.catalog_rows(Table::Spans), 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_large_memtable_flushes_without_waiting_for_age() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_bytes = ByteSize::mib(1);
        config.flush.max_age = Duration::from_secs(60 * 60);
        let storage = Storage::open(config).await.unwrap();
        let started = std::time::Instant::now();
        storage
            .append_spans(project(), spans_named(&[1], &"m".repeat(1_200_000)))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_flushes_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_secs(60 * 60);
        let storage = Storage::open(config.clone()).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.replayed(Table::Spans), 0);
        assert_eq!(storage.published_rows(Table::Spans), 0);
        assert_eq!(storage.catalog_rows(Table::Spans), 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_closes_the_catalog_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_secs(60 * 60);
        let storage = Storage::open(config).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        // The shutdown flush commits, so the catalog has WAL frames to checkpoint.
        storage.shutdown().await.unwrap();
        // Closing the last connection checkpointed the WAL into `catalog.db` and
        // deleted it: the file is final while `storage` is still alive.
        assert!(!dir.path().join("catalog.db-wal").exists());
        let catalog = std::fs::read(dir.path().join("catalog.db")).unwrap();
        assert!(matches!(
            storage.committer.staged_paths().await,
            Err(CatalogError::Closed)
        ));
        // A second shutdown has nothing left to close.
        storage.shutdown().await.unwrap();
        drop(storage);
        assert_eq!(
            std::fs::read(dir.path().join("catalog.db")).unwrap(),
            catalog
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn truncation_deletes_covered_segments_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.wal.segment_bytes = ByteSize::mib(1);
        config.flush.max_bytes = ByteSize::mib(1);
        config.flush.max_age = Duration::from_secs(60 * 60);
        config.memtable.hard_limit = ByteSize::mib(64);
        let storage = Storage::open(config).await.unwrap();
        storage.hooks().hold(HookPoint::FlushAfterFreeze);
        let big = incompressible(2 * 1024 * 1024);
        storage
            .append_spans(project(), spans_named(&[1], &big))
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_named(&[2], &big))
            .await
            .unwrap();
        let before = span_segments(dir.path());
        assert!(before.len() >= 2, "{before:?}");
        storage.hooks().release(HookPoint::FlushAfterFreeze);
        storage.shutdown().await.unwrap();
        let after = span_segments(dir.path());
        let oldest = before.iter().min().unwrap();
        let newest = before.iter().max().unwrap();
        assert!(!after.contains(oldest), "deleted {oldest:?} from {after:?}");
        assert!(after.contains(newest), "kept {newest:?} in {after:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flush_retries_failed_uploads_without_duplicating_rows() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as _);
        fault.push(
            Call::Put,
            test_support::Fault::Before(test_support::Status::SlowDown),
        );
        fault.push(
            Call::Put,
            test_support::Fault::Before(test_support::Status::SlowDown),
        );
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        config.flush.upload_concurrency = 1;
        let storage = Storage::open_with_store(config, Arc::new(fault), PutSizes::S3)
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        let committed: Vec<String> = storage
            .catalog_files(Table::Spans)
            .iter()
            .map(|file| file.path.clone())
            .collect();
        // Each failed attempt staged a spans file and its traces file.
        let staged = storage.staged_paths().await.unwrap();
        assert_eq!(staged.len(), 4, "{staged:?}");
        assert!(committed.iter().all(|path| !staged.contains(path)));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ambiguous_upload_is_retried_under_a_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as _);
        fault.push(
            Call::Put,
            test_support::Fault::After(test_support::Status::SlowDown),
        );
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        config.flush.upload_concurrency = 1;
        let storage = Storage::open_with_store(config, Arc::new(fault), PutSizes::S3)
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        let committed = storage.catalog_files(Table::Spans);
        assert_eq!(committed.len(), 1);
        // One failed attempt staged a spans file and its traces file. With
        // serial uploads the ambiguous failure hits the first (spans) object,
        // so only that object was written before the error.
        let staged = storage.staged_paths().await.unwrap();
        assert_eq!(staged.len(), 2, "{staged:?}");
        assert!(staged.iter().all(|path| path != &committed[0].path));
        let spans_staged = staged
            .iter()
            .find(|path| path.starts_with("tables/spans/"))
            .unwrap();
        inner
            .get(&object_store::path::Path::from(spans_staged.as_str()))
            .await
            .unwrap();
        inner
            .get(&object_store::path::Path::from(committed[0].path.as_str()))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flush_writes_a_traces_file_per_spans_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        let storage = Storage::open(config.clone()).await.unwrap();
        storage
            .append_spans(project(), three_traces_two_days())
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 3).await;
        assert_eq!(storage.catalog_paths(FileTable::Spans).len(), 2);
        assert_eq!(storage.catalog_paths(FileTable::Traces).len(), 2);
        let pairs = source_pairs(dir.path()).await;
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        for (spans_path, traces_path) in &pairs {
            let spans_batch = read_parquet(dir.path(), spans_path);
            let expected = partials_from_spans_batch(&spans_batch).unwrap();
            let actual = read_parquet(dir.path(), traces_path);
            assert_same_rows(&actual, &expected, traces_path);
        }
        storage.shutdown().await.unwrap();

        // Reopening loads the catalog (the open-time sources check passes) and
        // the traces files are in the published read state.
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.catalog_paths(FileTable::Spans).len(), 2);
        assert_eq!(storage.catalog_paths(FileTable::Traces).len(), 2);
        assert_eq!(source_pairs(dir.path()).await.len(), 2);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ambiguous_traces_upload_is_retried_under_new_names() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as _);
        // Serial uploads: the spans upload consumes the delay, the traces
        // upload writes the object and then fails ambiguously.
        fault.push(Call::Put, test_support::Fault::Delay(Duration::ZERO));
        fault.push(
            Call::Put,
            test_support::Fault::After(test_support::Status::SlowDown),
        );
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        config.flush.upload_concurrency = 1;
        let storage = Storage::open_with_store(config, Arc::new(fault), PutSizes::S3)
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        let committed = [
            storage.catalog_paths(FileTable::Spans),
            storage.catalog_paths(FileTable::Traces),
        ]
        .concat();
        assert_eq!(committed.len(), 2, "{committed:?}");
        let staged = storage.staged_paths().await.unwrap();
        assert_eq!(staged.len(), 2, "{staged:?}");
        assert!(staged.iter().all(|path| !committed.contains(path)));
        // The first attempt's spans object succeeded and its traces object was
        // written before the ambiguous failure; both are staged orphans.
        for path in &staged {
            inner
                .get(&object_store::path::Path::from(path.as_str()))
                .await
                .unwrap();
        }
        storage.collect_staged(Duration::ZERO).await.unwrap();
        assert!(storage.staged_paths().await.unwrap().is_empty());
        for path in &staged {
            assert!(
                inner
                    .get(&object_store::path::Path::from(path.as_str()))
                    .await
                    .is_err(),
                "{path} survived collection"
            );
        }
        for path in &committed {
            inner
                .get(&object_store::path::Path::from(path.as_str()))
                .await
                .unwrap();
        }
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_uploads_backpressure_until_the_store_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as _);
        fault.hold(&[Call::Put, Call::MultipartStart]);
        let mut config = config_in(dir.path());
        config.memtable.hard_limit = ByteSize::mib(16);
        config.flush.max_bytes = ByteSize::mib(1);
        config.flush.max_age = Duration::from_secs(60 * 60);
        config.flush.upload_concurrency = 1;
        let storage = Storage::open_with_store(config, Arc::new(fault.clone()), PutSizes::S3)
            .await
            .unwrap();
        let payload = incompressible(1024 * 1024);
        let mut accepted = 0_i64;
        loop {
            match storage
                .append_spans(project(), spans_named(&[accepted], &payload))
                .await
            {
                Ok(_) => accepted += 1,
                Err(AppendError::Backpressure) => break,
                Err(error) => panic!("{error}"),
            }
            assert!(accepted < 64, "backpressure never tripped");
        }
        assert!(accepted > 0);
        fault.release();
        let started = std::time::Instant::now();
        loop {
            match storage.append_spans(project(), spans_at(&[99])).await {
                Ok(_) => break,
                Err(AppendError::Backpressure) => {
                    assert!(started.elapsed() < Duration::from_secs(30));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("{error}"),
            }
        }
        storage.shutdown().await.unwrap();
    }

    fn incompressible(len: usize) -> String {
        let mut out = String::with_capacity(len);
        let mut state = 0x1234_5678_u64;
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            out.push(char::from(b'a' + ((state >> 33) as u8 % 26)));
        }
        out
    }

    fn span_segments(data_dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(data_dir.join("wal/spans")).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if parse_segment_name(&name.to_string_lossy()).is_some() {
                paths.push(entry.path());
            }
        }
        paths
    }

    /// A remote store with the read cache on, the flush cadence of the
    /// flush tests, and a cache of 64 MiB.
    fn cached_config(dir: &Path) -> StorageConfig {
        let mut config = config_in(dir);
        config.flush.max_age = Duration::from_millis(100);
        config.query.read_cache_bytes = Some(ByteSize::mib(64));
        config
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flushed_file_is_cached_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let storage = Storage::open_with_store(cached_config(dir.path()), inner, PutSizes::S3)
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        assert!(storage.read_cache_stats().unwrap().blocks > 0);

        // The file is read through the query engine's cache.
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let rows: usize = query
            .collect_raw_spans()
            .await
            .unwrap()
            .iter()
            .map(Batch::num_rows)
            .sum();
        assert_eq!(rows, 1);
        let stats = storage.read_cache_stats().unwrap();
        assert_eq!(stats.misses, 0, "the query fetched from S3: {stats:?}");
        assert!(stats.hits > 0);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn verification_reads_the_store_not_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(object_store::memory::InMemory::new());
        let storage = Storage::open_with_store(cached_config(dir.path()), inner, PutSizes::S3)
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        let file = storage.catalog_files(FileTable::Spans)[0].clone();

        // Replace block 0 of the cached copy with altered bytes and a CRC that
        // matches them, so the cache alone would accept it.
        let name = Path::new(&file.path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let block = dir.path().join("cache").join(&name).join("0");
        let mut bytes = std::fs::read(&block).unwrap();
        bytes[0] ^= 0xFF;
        let data_len = bytes.len() - 4;
        let crc = crc32c::crc32c(&bytes[..data_len]);
        bytes[data_len..].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&block, &bytes).unwrap();

        assert_eq!(
            storage.verify_queue().suspect([file.id], catalog::now_ns()),
            1
        );
        let report = storage.verify_pass().await;
        assert_eq!(report.healthy, 1, "{report:?}");
        assert_eq!(report.quarantined, 0);
        storage.shutdown().await.unwrap();
    }

    #[test]
    fn the_data_dir_layout_is_durable_after_a_crash_before_its_sync() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
        // A first open that was killed after it created `wal/` and its two
        // segment directories, before it synced any of them. The weaker state,
        // with only `wal/` unsynced, is masked: `lock_dir` syncs `data_dir`
        // during the same open, and `ensure_dir(wal/scores)` syncs `wal/`, so
        // a crash after that open cannot lose the layout.
        fs::create_dir(data.join("wal")).unwrap();
        fs::create_dir(data.join("wal").join(Table::Spans.name())).unwrap();
        fs::create_dir(data.join("wal").join(Table::Scores.name())).unwrap();

        let config = config_in(&data);
        test_support::crashed_life(&config, async |storage| {
            storage
                .append_spans(project(), spans_at(&[1]))
                .await
                .unwrap();
        });
        lossy.crash_with(fs::lossy::CrashMode::DropAll, 0);

        test_support::clean_life(&config, async |storage| {
            let query = storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap();
            assert_eq!(
                span_names(query).await.len(),
                1,
                "the acknowledged span is gone with the wal directory"
            );
        });
    }

    #[test]
    fn gc_leaves_no_orphan_after_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
        let mut config = config_in(&data);
        config.compaction.interval = Duration::from_secs(3_600);
        config.compaction.l0_files = 2;

        // Three lives, each appending one span of trace 1 and shutting down:
        // three L0 spans files.
        let start = NaiveDate::from_ymd_opt(2026, 3, 5)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap();
        for index in 0..3_u8 {
            let at = start + i64::from(index) * 60_000_000_000;
            test_support::clean_life(&config, async |storage| {
                let span = span_row(0, index, at, at + 1_000_000_000);
                storage
                    .append_spans(project(), span_rows(&[span]))
                    .await
                    .unwrap();
            });
        }

        // A fourth life compacts them, removes the inputs, and crashes.
        let noon = start + 2 * 3_600_000_000_000;
        test_support::crashed_life(&config, async |storage| {
            storage.compact(noon).await.unwrap();
            storage.collect_obsolete(Duration::ZERO).await.unwrap();
        });
        lossy.crash_with(fs::lossy::CrashMode::DropAll, 0);

        test_support::clean_life(&config, async |_storage| {
            let orphans = test_support::orphan_objects(&data).await;
            assert!(orphans.is_empty(), "orphan objects after GC: {orphans:?}");
        });
    }

    /// WAL config of the R2.4 tests: interval sync of one hour, so the writer
    /// syncs only when it must, and 1 MiB segments.
    fn interval_wal_config(dir: &Path) -> StorageConfig {
        let mut config = config_in(dir);
        config.wal.sync = WalSync::Interval(Duration::from_secs(3_600));
        config.wal.segment_bytes = ByteSize::mib(1);
        config
    }

    #[test]
    fn interval_rotation_syncs_the_old_segment() {
        for seed in 0..8_u64 {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("data");
            let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
            let config = interval_wal_config(&data);
            let big_id = span_row(1, 0, 1_000, 1_000).span_id.as_bytes().to_vec();

            // The 1.5 MiB group rotates the WAL. Its segment is never synced
            // by the writer in interval mode, so only the rotation can sync it.
            test_support::crashed_life(&config, async |storage| {
                storage
                    .append_spans(project(), test_support::big_span_batch(1, 1_572_864))
                    .await
                    .unwrap();
                assert_eq!(span_segments(&data).len(), 2, "the group rotated the WAL");
                storage
                    .append_spans(project(), span_rows(&[span_row(2, 0, 2_000, 2_000)]))
                    .await
                    .unwrap();
            });
            lossy.crash_with(fs::lossy::CrashMode::TearLast, seed);

            test_support::clean_life(&config, async |storage| {
                let ids = span_ids(storage).await;
                assert!(
                    ids.contains(&big_id),
                    "seed {seed}: the big span is gone after the crash ({} spans stored)",
                    ids.len()
                );
            });
        }
    }

    /// A lossy crash that keeps part of the unsynced tail after an interval
    /// rotation: the segment the rotation synced is whole, and the store reopens.
    /// Sixteen seeds per `CETRA_LOSSY_SEEDS`.
    #[test]
    fn a_lossy_crash_after_an_interval_rotation_reopens() {
        for seed in 0..16 * crash_tests::seeds() {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("data");
            let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
            let config = interval_wal_config(&data);
            let big_id = span_row(1, 0, 1_000, 1_000).span_id.as_bytes().to_vec();

            test_support::crashed_life(&config, async |storage| {
                storage
                    .append_spans(project(), test_support::big_span_batch(1, 1_572_864))
                    .await
                    .unwrap();
                assert_eq!(span_segments(&data).len(), 2, "the group rotated the WAL");
                storage
                    .append_spans(project(), span_rows(&[span_row(2, 0, 2_000, 2_000)]))
                    .await
                    .unwrap();
            });
            lossy.crash_with(fs::lossy::CrashMode::KeepRandom, seed);

            test_support::clean_life(&config, async |storage| {
                let ids = span_ids(storage).await;
                assert!(
                    ids.contains(&big_id),
                    "seed {seed}: the big span is gone after the crash ({} spans stored)",
                    ids.len()
                );
            });
        }
    }

    /// The last segment cut to each length of a torn header, after an interval
    /// rotation. The rows of the synced segment read back, and the store reopens
    /// and keeps appending. A header cut short cannot happen once it is synced,
    /// but a torn one must still be handled (spec 03 § 1).
    #[test]
    fn a_torn_last_header_keeps_the_rows_before_it() {
        for cut in 0..=SEGMENT_HEADER_LEN {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("data");
            let config = interval_wal_config(&data);
            let big_id = span_row(1, 0, 1_000, 1_000).span_id.as_bytes().to_vec();

            test_support::crashed_life(&config, async |storage| {
                storage
                    .append_spans(project(), test_support::big_span_batch(1, 1_572_864))
                    .await
                    .unwrap();
                // Unsynced, in the last segment: the cut below may take it too.
                storage
                    .append_spans(project(), span_rows(&[span_row(2, 0, 2_000, 2_000)]))
                    .await
                    .unwrap();
            });
            let mut segments = span_segments(&data);
            segments.sort();
            let last = segments.last().expect("the group rotated the WAL").clone();
            assert_eq!(segments.len(), 2, "cut {cut}");
            OpenOptions::new()
                .write(true)
                .open(&last)
                .unwrap()
                .set_len(cut as u64)
                .unwrap();

            test_support::clean_life(&config, async |storage| {
                let ids = span_ids(storage).await;
                assert!(ids.contains(&big_id), "cut {cut}: the synced rows are gone");
                storage
                    .append_spans(project(), span_rows(&[span_row(3, 0, 3_000, 3_000)]))
                    .await
                    .unwrap();
            });
            test_support::clean_life(&config, async |storage| {
                let ids = span_ids(storage).await;
                assert!(
                    ids.contains(&big_id),
                    "cut {cut}: reopen lost the synced rows"
                );
            });
        }
    }

    #[test]
    fn reopen_syncs_the_replayed_segment() {
        for seed in 0..8_u64 {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("data");
            let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
            let config = interval_wal_config(&data);

            test_support::crashed_life(&config, async |storage| {
                storage
                    .append_spans(project(), span_rows(&[span_row(1, 0, 1_000, 1_000)]))
                    .await
                    .unwrap();
            });
            // The second open replays the segment and creates a newer one; it
            // appends nothing, so the replayed segment is no longer the last.
            test_support::crashed_life(&config, async |_storage| {});
            lossy.crash_with(fs::lossy::CrashMode::TearLast, seed);

            test_support::clean_life(&config, async |storage| {
                assert_eq!(
                    span_ids(storage).await.len(),
                    1,
                    "seed {seed}: the first span is gone after the crash"
                );
            });
        }
    }

    /// The names of the `wal/spans` segments, sorted.
    fn span_segment_names(data_dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = span_segments(data_dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// WAL config of the R2.5 tests: every group is synced before its ack.
    fn always_wal_config(dir: &Path) -> StorageConfig {
        let mut config = config_in(dir);
        config.wal.sync = WalSync::Always;
        config.wal.segment_bytes = ByteSize::mib(1);
        config
    }

    #[test]
    fn a_failed_sync_leaves_the_group_and_does_not_sync_again() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let config = always_wal_config(&data);

        test_support::crashed_life(&config, async |storage| {
            storage.hooks().arm_fail(HookPoint::WalSync);
            let error = storage
                .append_spans(project(), span_rows(&[span_row(1, 0, 1_000, 1_000)]))
                .await
                .unwrap_err();
            assert_eq!(error, AppendError::WalFailed);
            assert_eq!(storage.hooks().hits(HookPoint::WalSync), 1);
        });

        // Nothing was rewound: the page cache still holds the group, so a
        // restart without a power loss replays it.
        test_support::clean_life(&config, async |storage| {
            assert_eq!(
                span_ids(storage).await.len(),
                1,
                "the failed group was rewound although its fsync only failed"
            );
        });
    }

    #[test]
    fn a_failed_sync_group_is_lost_on_power_loss() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
        let config = always_wal_config(&data);

        test_support::crashed_life(&config, async |storage| {
            storage.hooks().arm_fail(HookPoint::WalSync);
            let error = storage
                .append_spans(project(), span_rows(&[span_row(1, 0, 1_000, 1_000)]))
                .await
                .unwrap_err();
            assert_eq!(error, AppendError::WalFailed);
        });
        lossy.crash_with(fs::lossy::CrashMode::DropAll, 0);

        test_support::clean_life(&config, async |storage| {
            assert!(
                span_ids(storage).await.is_empty(),
                "an unsynced group survived a power loss"
            );
        });
    }

    #[test]
    fn a_discarded_segment_stays_removed_after_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
        let config = always_wal_config(&data);
        let first_path = data.join("wal/spans").join(segment_name(1));
        let mut first = Vec::new();
        let mut old_len = 0;

        // The 1.5 MiB group syncs, rotates to segment 2, and then the insert
        // fails. The writer discards segment 2 and stops.
        test_support::crashed_life(&config, async |storage| {
            first = span_segment_names(&data);
            assert_eq!(first, vec![segment_name(1)]);
            old_len = fs::metadata(&first_path).unwrap().len();
            storage.hooks().arm_fail(HookPoint::AfterWalSync);
            let error = storage
                .append_spans(project(), test_support::big_span_batch(1, 1_572_864))
                .await
                .unwrap_err();
            assert_eq!(error, AppendError::WalFailed);
            assert_eq!(storage.hooks().hits(HookPoint::AfterWalSync), 1);
            assert_eq!(span_segment_names(&data), first);
            assert_eq!(fs::metadata(&first_path).unwrap().len(), old_len);
        });
        lossy.crash_with(fs::lossy::CrashMode::DropAll, 0);

        // The unlink of segment 2 was not synced, so the power loss may bring
        // its empty file back. The discarded segment must stay gone.
        assert_eq!(
            span_segment_names(&data),
            first,
            "the discarded segment came back after the power loss"
        );
        test_support::clean_life(&config, async |_storage| {});
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_flush_fsync_makes_storage_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(500);
        let storage = Storage::open(config.clone()).await.unwrap();
        storage.hooks().arm_fail(HookPoint::ObjectSync);
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();

        let started = std::time::Instant::now();
        while !storage.health().objects_sync_failed {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "a failed flush fsync never reached health()"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();

        let stopping = std::time::Instant::now();
        storage.shutdown().await.unwrap();
        assert!(
            stopping.elapsed() < Duration::from_secs(5),
            "shutdown waited for flush retries that cannot succeed"
        );

        let storage = Storage::open(config).await.unwrap();
        assert!(!storage.health().objects_sync_failed);
        assert_eq!(span_ids(&storage).await.len(), 2);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_catalog_fsync_error_makes_storage_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(500);
        let storage = Storage::open(config.clone()).await.unwrap();
        storage.hooks().arm_fail(HookPoint::CatalogFsync);
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();

        let started = std::time::Instant::now();
        while !storage.health().catalog_failed {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "a failed catalog fsync never reached health()"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();

        let stopping = std::time::Instant::now();
        storage.shutdown().await.unwrap();
        assert!(
            stopping.elapsed() < Duration::from_secs(5),
            "shutdown waited for flush retries that cannot succeed"
        );

        let storage = Storage::open(config).await.unwrap();
        assert!(!storage.health().catalog_failed);
        assert_eq!(span_ids(&storage).await.len(), 2);
        storage.shutdown().await.unwrap();
    }

    /// `flush_now` covers every append acknowledged before it started, while
    /// another task keeps appending. Twenty rounds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_covers_every_acknowledged_append() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(quiet_config_in(dir.path())).await.unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let acknowledged = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let appender = {
            let storage = Arc::clone(&storage);
            let stop = Arc::clone(&stop);
            let acknowledged = Arc::clone(&acknowledged);
            tokio::spawn(async move {
                let mut start = 0_i64;
                while !stop.load(Ordering::SeqCst) {
                    let receipt = storage
                        .append_spans(project(), spans_at(&[start]))
                        .await
                        .unwrap();
                    acknowledged.fetch_max(receipt.seq, Ordering::SeqCst);
                    start += 1;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
        };

        let mut rounds = 0;
        for round in 0..20 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let target = acknowledged.load(Ordering::SeqCst);
            if target == 0 {
                continue;
            }
            rounds += 1;
            storage
                .flush_now(Duration::from_secs(5))
                .await
                .unwrap_or_else(|error| panic!("round {round}: {error}"));
            assert!(
                storage.flushed_seq(Table::Spans).await >= target,
                "round {round}: seq {target} acknowledged but not flushed"
            );
        }
        assert!(rounds >= 10, "only {rounds} of 20 rounds checked a flush");

        stop.store(true, Ordering::SeqCst);
        appender.await.unwrap();
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// `flush_now` gives up once shutdown has begun, even while its flush is
    /// held: it returns `ShuttingDown` within its own timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_gives_up_once_shutdown_began() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(quiet_config_in(dir.path())).await.unwrap());
        let hooks = storage.hooks().clone();
        hooks.hold(HookPoint::FlushAfterWrite);
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let flushing = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.flush_now(Duration::from_secs(30)).await })
        };
        jobs::wait_at(&storage, HookPoint::FlushAfterWrite);
        storage.shutting_down.store(true, Ordering::Release);
        let flushed = tokio::time::timeout(Duration::from_secs(5), flushing).await;
        hooks.release(HookPoint::FlushAfterWrite);
        let flushed = flushed
            .expect("flush_now kept waiting after shutdown began")
            .unwrap();
        assert!(
            matches!(flushed, Err(FlushNowError::ShuttingDown)),
            "{flushed:?}"
        );
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// A `flush_now` racing `shutdown` returns `Ok` or `ShuttingDown` within its
    /// timeout, and `shutdown` still finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_racing_shutdown_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(quiet_config_in(dir.path())).await.unwrap());
        storage
            .append_spans(project(), spans_at(&[1, 2]))
            .await
            .unwrap();
        let flushing = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.flush_now(Duration::from_secs(5)).await })
        };
        let stopping = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.shutdown().await })
        };
        let flushed = tokio::time::timeout(Duration::from_secs(10), flushing)
            .await
            .expect("flush_now did not return")
            .unwrap();
        assert!(
            matches!(flushed, Ok(()) | Err(FlushNowError::ShuttingDown)),
            "{flushed:?}"
        );
        stopping.await.unwrap().unwrap();
    }

    /// A storage whose flush tasks do nothing on their own: one hour of age
    /// and 1 MiB of size. Only [`Storage::flush_now`] flushes.
    fn quiet_config_in(dir: &Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            flush: FlushConfig {
                max_age: Duration::from_secs(3600),
                max_bytes: ByteSize::mib(1),
                ..FlushConfig::default()
            },
            ..config_in(dir)
        }
    }

    /// Spans rows that `project` sees through a query.
    async fn span_count(storage: &Storage) -> i64 {
        let ctx = QueryContext::new(
            storage
                .query(project(), QueryOptions::new(None, None))
                .await
                .unwrap(),
            None,
        )
        .unwrap();
        let params = serde_json::Map::new();
        let res = ctx
            .sql_json(SqlRequest {
                query: "SELECT count(*) AS n FROM spans",
                params: &params,
                limit: 1,
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap();
        res.rows[0]["n"].as_i64().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_commits_everything_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        let spans = storage
            .append_spans(project(), spans_at(&[1, 2]))
            .await
            .unwrap();
        let scores = storage
            .append_scores(project(), score_batch("accuracy"))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        assert!(storage.flushed_seq(Table::Spans).await >= spans.seq);
        assert!(storage.flushed_seq(Table::Scores).await >= scores.seq);
        assert_eq!(storage.spans.shared().memtable_bytes(), 0);
        assert_eq!(storage.scores.shared().memtable_bytes(), 0);
        assert_eq!(span_count(&storage).await, 2);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_with_nothing_to_flush_returns_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        let before = (
            storage.flushed_seq(Table::Spans).await,
            storage.flushed_seq(Table::Scores).await,
        );
        let started = std::time::Instant::now();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let after = (
            storage.flushed_seq(Table::Spans).await,
            storage.flushed_seq(Table::Scores).await,
        );
        assert_eq!(before, after);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_does_not_wait_for_later_appends() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(quiet_config_in(dir.path())).await.unwrap());
        let first = storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let appender = {
            let storage = Arc::clone(&storage);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut start = 2_i64;
                while !stop.load(Ordering::Acquire) {
                    match storage.append_spans(project(), spans_at(&[start])).await {
                        Ok(_) | Err(AppendError::Backpressure) => {}
                        Err(error) => panic!("append failed: {error}"),
                    }
                    start += 1;
                    tokio::task::yield_now().await;
                }
            })
        };
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        assert!(storage.flushed_seq(Table::Spans).await >= first.seq);
        stop.store(true, Ordering::Release);
        appender.await.unwrap();
        let storage = Arc::try_unwrap(storage).unwrap();
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_clears_the_force_flag() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        assert!(!storage.spans.shared().force_flush());
        assert!(!storage.scores.shared().force_flush());
        storage
            .append_spans(project(), spans_at(&[2]))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Nothing forces the flush now, so the append waits in the memtable.
        assert!(storage.spans.shared().memtable_bytes() > 0);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_after_shutdown_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        storage.shutdown().await.unwrap();
        let error = storage.flush_now(Duration::from_secs(1)).await.unwrap_err();
        assert!(matches!(error, FlushNowError::ShuttingDown), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_reports_a_failed_writer() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.hooks().arm_fail(HookPoint::WalWrite);
        let mut failed = false;
        for start in 2..12 {
            if storage
                .append_spans(project(), spans_at(&[start]))
                .await
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed, "an append failed after the armed fault");
        let error = storage
            .flush_now(Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            matches!(error, FlushNowError::WriterFailed { table: "spans" }),
            "{error}"
        );
        let shutdown = storage.shutdown().await.unwrap_err();
        assert!(matches!(
            shutdown,
            StorageError::WalFailed { table: "spans" }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flush_now_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(quiet_config_in(dir.path())).await.unwrap();
        storage.hooks().hold(HookPoint::FlushAfterFreeze);
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let error = storage
            .flush_now(Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(
            matches!(error, FlushNowError::Timeout { table: "spans", .. }),
            "{error}"
        );
        storage.hooks().release(HookPoint::FlushAfterFreeze);
        storage.shutdown().await.unwrap();
    }

    #[test]
    fn project_of_and_replay_batch_handle_all_four_encodings_and_selection() {
        use cetra_column::{
            Batch, Column, DictColumn, FlatColumn, FlatData, RleColumn, ScalarValue, Selection,
            Utf8Data,
        };

        let base = spans_at(&[10, 20]);
        let pid_idx = base.schema().index_of(spans::PROJECT_ID).unwrap();
        let pid_str = project().to_string();
        let other_str = other_project().to_string();

        // 1. Flat
        assert_eq!(project_of(&base).unwrap(), project());

        // 2. Const
        let mut const_cols = base.columns().to_vec();
        const_cols[pid_idx] =
            Arc::new(Column::try_new_const(ScalarValue::Utf8(pid_str.clone()), 2).unwrap());
        let const_batch =
            Batch::try_new_shared(Arc::clone(base.schema()), const_cols, 2, Selection::All)
                .unwrap();
        assert_eq!(project_of(&const_batch).unwrap(), project());

        // 3. Dict (with Selection::Indices keeping only row 1 so mixed row 0 is ignored)
        let mut dict_cols = base.columns().to_vec();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices([other_str.as_str(), pid_str.as_str()]).unwrap(),
            ),
            None,
        )
        .unwrap();
        dict_cols[pid_idx] = Arc::new(Column::Dict(
            DictColumn::try_new(dict_vals, vec![0, 1], None).unwrap(),
        ));
        let dict_batch = Batch::try_new_shared(
            Arc::clone(base.schema()),
            dict_cols,
            2,
            Selection::Indices(vec![1]),
        )
        .unwrap();
        assert_eq!(project_of(&dict_batch).unwrap(), project());

        // 4. Rle
        let mut rle_cols = base.columns().to_vec();
        let rle_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices([pid_str.as_str()]).unwrap()),
            None,
        )
        .unwrap();
        rle_cols[pid_idx] = Arc::new(Column::Rle(RleColumn::try_new(rle_vals, vec![2]).unwrap()));
        let rle_batch =
            Batch::try_new_shared(Arc::clone(base.schema()), rle_cols, 2, Selection::All).unwrap();
        assert_eq!(project_of(&rle_batch).unwrap(), project());

        // Replay across all 4 encodings into a memtable.
        let mut memtable = Memtable::new(Table::Spans);
        replay_batch(&mut memtable, 1, base).unwrap();
        replay_batch(&mut memtable, 2, const_batch).unwrap();
        replay_batch(&mut memtable, 3, dict_batch).unwrap();
        replay_batch(&mut memtable, 4, rle_batch).unwrap();
        assert_eq!(memtable.max_seq(), 4);
    }
}
