//! Garbage collection: staged objects that were written and never committed,
//! and obsolete files no reader can still use.
//!
//! The task runs once at open, after replay, and then every [`INTERVAL`].
//! Staged-object collection reads `staged_objects` instead of listing the
//! bucket. A row younger than [`MAX_AGE`] stays, so a flush that has staged its
//! outputs and not yet committed them is left alone. Obsolete-file collection
//! reads the `obsolete` rows of `data_files`, honours `gc.grace`, and leaves
//! objects that an older catalog still references (spec 03 §5).

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::TryStreamExt;
use object_store::{ObjectMeta, ObjectStore, path::Path as ObjectPath};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::catalog::{Committer, FileId, ObsoleteRemoval, StagedRemoval, now_ns};
use crate::error::{GcError, OrphanError};
use crate::hooks::Hooks;
use crate::media::{MEDIA_INTERVAL, MediaGc};
use crate::objects::{Objects, is_temp_name};

/// How often the background task collects, after the pass at open.
pub(crate) const INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Staged rows older than this are eligible. Spec 03 §5.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Obsolete rows per `obsolete_before` page.
const PAGE: usize = 1000;

/// A running collection task. Drop cancels it.
#[derive(Debug)]
pub(crate) struct GcHandle {
    stop: watch::Sender<bool>,
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl GcHandle {
    /// Asks the task to stop and cancels it.
    pub(crate) fn abort(&self) {
        let _ = self.stop.send(true);
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// What the task needs. One value, so `spawn` stays under the argument limit.
pub(crate) struct GcStart {
    /// Catalog writer. Collection takes its connection for the row delete.
    pub committer: Arc<Committer>,
    /// Objects named by `staged_objects` and `data_files`.
    pub objects: Arc<Objects>,
    /// Test pause before a delete.
    pub hooks: Hooks,
    /// `storage.gc.grace`, how long an obsolete file outlives its commit.
    pub grace: Duration,
    /// Media collection, once a day (spec 03 §9).
    pub media: MediaGc,
}

/// Starts the periodic task. The caller already ran one pass at open.
pub(crate) fn spawn(start: GcStart) -> GcHandle {
    let (stop_tx, mut stop) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
        let mut next_media = tokio::time::Instant::now() + MEDIA_INTERVAL;
        loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = collect_once(
                        &start.committer,
                        &start.objects,
                        &start.hooks,
                        now_ns(),
                        MAX_AGE,
                    )
                    .await
                    {
                        // The error names the object key. Keys contain project ids.
                        tracing::warn!("staged object collection failed");
                        tracing::debug!(%error, "staged object collection error");
                    }
                    if let Err(error) = collect_obsolete(
                        &start.committer,
                        &start.objects,
                        &start.hooks,
                        now_ns(),
                        start.grace,
                    )
                    .await
                    {
                        // The error names the object key. Keys contain project ids.
                        tracing::warn!("obsolete file collection failed");
                        tracing::debug!(%error, "obsolete file collection error");
                    }
                    if tokio::time::Instant::now() >= next_media {
                        if let Err(error) = start.media.collect_all(now_ns()).await {
                            // The label only: the error names an object path,
                            // which holds the blob's hash (spec 03 §9).
                            tracing::warn!(
                                error = error.label(),
                                "media collection failed"
                            );
                        }
                        next_media = tokio::time::Instant::now() + MEDIA_INTERVAL;
                    }
                    // Half-finished drops get their media step retried
                    // (spec 03 §7).
                    start.media.finish_drops().await;
                }
            }
        }
    });
    GcHandle {
        stop: stop_tx,
        task: std::sync::Mutex::new(Some(task)),
    }
}

/// Deletes staged objects with `created_at` strictly before `now - max_age`.
pub(crate) async fn collect_once(
    committer: &Committer,
    objects: &Objects,
    hooks: &Hooks,
    now: i64,
    max_age: Duration,
) -> Result<(), GcError> {
    let cutoff = cutoff_ns(now, max_age);
    let paths = committer.staged_before(cutoff).await?;
    let mut deleted = 0_u64;
    let mut bytes = 0_u64;
    let mut conflicts = 0_u64;
    for path in &paths {
        match committer
            .collect_staged_path(path, hooks, cutoff, || objects.delete_len(path))
            .await?
        {
            StagedRemoval::Deleted { bytes: size } => {
                deleted += 1;
                bytes = bytes.saturating_add(size);
                tracing::debug!(path, "deleted staged object");
            }
            StagedRemoval::Published => {
                conflicts += 1;
                tracing::error!("staged object is also a catalog file; left in place");
                tracing::debug!(path, "staged path is present in data_files");
            }
            StagedRemoval::MediaStored => {
                // No fields: the path holds the blob's hash (spec 03 §9).
                tracing::debug!("stale staged row of a stored media blob removed");
            }
            StagedRemoval::Gone => {}
        }
    }
    if deleted > 0 || conflicts > 0 {
        tracing::info!(deleted, bytes, conflicts, "collected staged objects");
    }
    Ok(())
}

fn cutoff_ns(now: i64, max_age: Duration) -> i64 {
    let age = i64::try_from(max_age.as_nanos()).unwrap_or(i64::MAX);
    now.saturating_sub(age)
}

/// What one obsolete-file collection did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObsoleteReport {
    /// Objects deleted, with their rows.
    pub deleted: u64,
    /// Bytes of the deleted objects.
    pub bytes: u64,
    /// Files skipped because a reader still references them.
    pub in_use: u64,
}

/// Deletes obsolete files whose `obsoleted_at + grace` has passed, unless a
/// reader of this process still references them (spec 03 §5).
pub(crate) async fn collect_obsolete(
    committer: &Committer,
    objects: &Objects,
    hooks: &Hooks,
    now: i64,
    grace: Duration,
) -> Result<ObsoleteReport, GcError> {
    let cutoff = cutoff_ns(now, grace);
    let mut report = ObsoleteReport::default();
    let mut after: Option<(i64, FileId)> = None;
    loop {
        let page = committer
            .obsolete_before(cutoff, after, PAGE as u32)
            .await?;
        for file in &page {
            // Taken outside the committer lock. Files a backup's verify pass
            // quarantines leave the catalog without entering `retired`, so
            // the gate is what keeps them (N M2, D-N-18).
            let removal = {
                let _gate = committer.object_gc_gate.read().await;
                committer
                    .collect_obsolete_file(file, hooks, || objects.delete_len(&file.path))
                    .await?
            };
            match removal {
                ObsoleteRemoval::Deleted { bytes } => {
                    report.deleted += 1;
                    report.bytes = report.bytes.saturating_add(bytes);
                    tracing::debug!(path = file.path, "deleted obsolete file");
                }
                ObsoleteRemoval::InUse => report.in_use += 1,
                ObsoleteRemoval::Gone => {}
            }
        }
        let Some(last) = page.last() else {
            break;
        };
        after = Some((last.obsoleted_at, last.id));
        if page.len() < PAGE {
            break;
        }
    }
    if report.deleted + report.in_use > 0 {
        tracing::info!(
            deleted = report.deleted,
            bytes = report.bytes,
            in_use = report.in_use,
            "collected obsolete files"
        );
    }
    Ok(report)
}

/// What [`crate::Maintenance::collect_orphans`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReport {
    /// Unknown objects older than `min_age` (deleted unless `dry_run`).
    pub orphans: u64,
    /// Bytes of the objects counted in `orphans`.
    pub orphan_bytes: u64,
    /// Unknown objects younger than `min_age`, left alone.
    pub recent_unknown: u64,
    /// Data files the catalog needs that the store does not have.
    pub missing: u64,
    /// Media blobs the catalog references that the store does not have (a warning:
    /// media follows its own deletion rule, see N M2's design § Backup layout).
    pub missing_media: u64,
    /// Whether the run only reported and deleted nothing.
    pub dry_run: bool,
}

/// Lists `tables/` and `media/` in the store. Deletes the objects the catalog does
/// not know that are older than `min_age` (unless `dry_run`), and counts the
/// objects it needs that are missing (N M2, D-N-21). The periodic GC never lists.
pub(crate) async fn collect_orphans(
    committer: &Committer,
    objects: &Objects,
    min_age: Duration,
    dry_run: bool,
) -> Result<OrphanReport, OrphanError> {
    // Taken before the committer lock (the lock order of N M2, D-N-18). Object
    // deletions by the GC, media GC and a snapshot wait for it.
    let _gate = committer.object_gc_gate.write().await;
    // Needed is read before the listing and known after it: an object published
    // during the listing is then known, never an orphan.
    let mut missing = committer.needed_keys().await?;
    let store = objects.raw_store();
    let mut listed: Vec<ObjectMeta> = Vec::new();
    for prefix in ["tables", "media"] {
        let mut stream = store.list(Some(&ObjectPath::from(prefix)));
        while let Some(meta) = stream.try_next().await.map_err(OrphanError::List)? {
            listed.push(meta);
        }
    }
    let known = committer.known_keys().await?;
    let now = SystemTime::now();
    let mut report = OrphanReport {
        dry_run,
        ..OrphanReport::default()
    };
    for meta in &listed {
        let key = meta.location.as_ref();
        let segment = key.rsplit('/').next().unwrap_or(key);
        if is_temp_name(OsStr::new(segment)) {
            continue;
        }
        missing.remove(key);
        if known.contains(key) {
            continue;
        }
        // A modification time in the future (clock skew) counts as recent, so
        // the object is never deleted, whatever `min_age` is.
        let recent = match now.duration_since(SystemTime::from(meta.last_modified)) {
            Ok(age) => age < min_age,
            Err(_) => true,
        };
        if recent {
            report.recent_unknown += 1;
            continue;
        }
        report.orphans += 1;
        report.orphan_bytes = report.orphan_bytes.saturating_add(meta.size);
        if !dry_run {
            objects
                .delete(key)
                .await
                .map_err(|error| OrphanError::delete(&error))?;
        }
    }
    for key in &missing {
        if key.starts_with("media/") {
            report.missing_media += 1;
        } else {
            report.missing += 1;
        }
    }
    tracing::info!(
        orphans = report.orphans,
        orphan_bytes = report.orphan_bytes,
        recent_unknown = report.recent_unknown,
        missing = report.missing,
        missing_media = report.missing_media,
        dry_run,
        "collected orphan objects"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use bytesize::ByteSize;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use chrono::NaiveDate;
    use llmobs_core::ids::ProjectId;
    use sqlx::ConnectOptions;

    use futures::TryStreamExt;
    use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
    use sha2::{Digest, Sha256};

    use super::{ObsoleteReport, OrphanReport, collect_obsolete, collect_once};
    use crate::MediaBlob;
    use crate::catalog::{CatalogEdit, Committer, FileId, FileMeta, FileState, FileTable, now_ns};
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::error::GcError;
    use crate::hooks::{HookPoint, Hooks};
    use crate::objects::Objects;
    use crate::wal::{Table, spans_at};

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn config_in(dir: &Path) -> StorageConfig {
        StorageConfig {
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

    fn write_object(dir: &Path, path: &str, bytes: &[u8]) {
        let file = dir.join("objects").join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, bytes).unwrap();
    }

    fn object_exists(dir: &Path, path: &str) -> bool {
        dir.join("objects").join(path).exists()
    }

    async fn collect(storage: &crate::Storage, now: i64, max_age: Duration) {
        collect_once(
            &storage.committer,
            &storage.objects,
            &storage.hooks,
            now,
            max_age,
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_old_object_is_collected_and_a_young_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let now = now_ns();
        let old = "tables/spans/old.parquet";
        let young = "tables/spans/young.parquet";
        write_object(dir.path(), old, b"old-bytes");
        write_object(dir.path(), young, b"young-bytes");
        storage
            .committer
            .stage_at(&[old.to_owned()], now - 2_000_000_000)
            .await
            .unwrap();
        storage
            .committer
            .stage_at(&[young.to_owned()], now)
            .await
            .unwrap();

        collect(&storage, now, Duration::from_secs(1)).await;

        assert_eq!(
            storage.staged_paths().await.unwrap(),
            vec![young.to_owned()]
        );
        assert!(!object_exists(dir.path(), old));
        assert!(object_exists(dir.path(), young));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_staged_row_without_an_object_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let now = now_ns();
        let path = "tables/spans/missing.parquet";
        storage
            .committer
            .stage_at(&[path.to_owned()], now - 2_000_000_000)
            .await
            .unwrap();

        collect(&storage, now, Duration::from_secs(1)).await;

        assert!(storage.staged_paths().await.unwrap().is_empty());
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_catalog_file_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        let storage = crate::Storage::open(config).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        let started = Instant::now();
        let path = loop {
            let files = storage.catalog_files(Table::Spans);
            if files.len() == 1 && storage.published_rows(Table::Spans) == 0 {
                break files[0].path.clone();
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "flush did not publish the span"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let now = now_ns();
        storage
            .committer
            .stage_at(std::slice::from_ref(&path), now - 2_000_000_000)
            .await
            .unwrap();

        collect(&storage, now, Duration::from_secs(1)).await;

        assert!(object_exists(dir.path(), &path));
        assert_eq!(storage.staged_paths().await.unwrap(), vec![path]);
        assert_eq!(storage.catalog_rows(Table::Spans), 1);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collection_during_a_flush_keeps_the_fresh_staged_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path());
        config.flush.max_age = Duration::from_millis(100);
        let storage = crate::Storage::open(config).await.unwrap();
        let hooks = storage.hooks().clone();
        hooks.hold(HookPoint::FlushAfterStage);
        storage
            .append_spans(project(), spans_at(&[1, 2]))
            .await
            .unwrap();
        let started = Instant::now();
        while storage.hooks().waiting_at(HookPoint::FlushAfterStage) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "flush did not stage"
            );
            tokio::task::yield_now().await;
        }
        // One partition now stages a spans file and its derived traces file.
        let fresh = storage.staged_paths().await.unwrap();
        assert_eq!(fresh.len(), 2);

        let now = now_ns();
        let old = "tables/spans/leftover.parquet";
        write_object(dir.path(), old, b"leftover");
        storage
            .committer
            .stage_at(&[old.to_owned()], now - 2_000_000_000)
            .await
            .unwrap();
        collect(&storage, now, Duration::from_secs(1)).await;

        let mut staged = storage.staged_paths().await.unwrap();
        staged.retain(|path| path != old);
        assert_eq!(staged, fresh);
        assert!(!object_exists(dir.path(), old));
        hooks.release(HookPoint::FlushAfterStage);
        storage.shutdown().await.unwrap();

        let object = dir.path().join("objects").join(&fresh[0]);
        assert!(
            object.is_file(),
            "flush output was deleted during collection"
        );
    }

    // ---- Obsolete-file collection (M4.1) ----

    const SPANS_BYTES: &[u8] = b"spans-object-bytes";
    const TRACES_BYTES: &[u8] = b"traces-bytes";

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    fn meta(table: FileTable, id: FileId, path: String) -> FileMeta {
        FileMeta {
            id,
            table,
            project: project(),
            day: day(),
            level: 0,
            path,
            size: 1,
            footer_size: 1,
            rows: 1,
            min_time: 1,
            max_time: 2,
            min_ingest_time: 3,
            max_ingest_time: 4,
            fully_merged: false,
            deletions_applied_seq: 0,
        }
    }

    /// A directory with a committer and its local object store.
    struct Fixture {
        dir: tempfile::TempDir,
        committer: Committer,
        objects: Arc<Objects>,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let committer = Committer::open(dir.path(), Hooks::default()).await.unwrap();
        let objects = Arc::new(
            Objects::new(&config_in(dir.path()), Hooks::default())
                .await
                .unwrap(),
        );
        Fixture {
            dir,
            committer,
            objects,
        }
    }

    /// Stages, commits and (optionally) writes a spans file and the traces file
    /// derived from it. Returns `((spans id, traces id), (spans path, traces path))`.
    async fn add_pair(f: &Fixture, write_objects: bool) -> ((FileId, FileId), (String, String)) {
        let (spans_path, spans_id) =
            Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let (traces_path, traces_id) =
            Objects::table_file_path(FileTable::Traces, project(), day(), 0);
        let edit = CatalogEdit {
            add: vec![
                meta(FileTable::Spans, spans_id, spans_path.clone()),
                meta(FileTable::Traces, traces_id, traces_path.clone()),
            ],
            remove: Vec::new(),
            flushed_seq: None,
            sources: vec![(traces_id, spans_id)],
            raise: Vec::new(),
            quarantine: Vec::new(),
        };
        f.committer
            .stage(&[spans_path.clone(), traces_path.clone()])
            .await
            .unwrap();
        f.committer.commit(&edit, |_| {}).await.unwrap();
        if write_objects {
            f.objects
                .put_durable(&spans_path, Bytes::from_static(SPANS_BYTES))
                .await
                .unwrap();
            f.objects
                .put_durable(&traces_path, Bytes::from_static(TRACES_BYTES))
                .await
                .unwrap();
        }
        ((spans_id, traces_id), (spans_path, traces_path))
    }

    async fn remove_pair(f: &Fixture, spans_id: FileId, traces_id: FileId) {
        let remove = CatalogEdit {
            add: Vec::new(),
            remove: vec![
                (spans_id, FileState::Active),
                (traces_id, FileState::Active),
            ],
            flushed_seq: None,
            sources: Vec::new(),
            raise: Vec::new(),
            quarantine: Vec::new(),
        };
        f.committer.commit(&remove, |_| {}).await.unwrap();
    }

    /// The pair setup of the plan: added, written, then obsoleted.
    async fn obsolete_pair(f: &Fixture) -> ((FileId, FileId), (String, String)) {
        let (ids, paths) = add_pair(f, true).await;
        remove_pair(f, ids.0, ids.1).await;
        (ids, paths)
    }

    /// One spans file, no traces file; `write_object` controls its object.
    async fn obsolete_one(f: &Fixture, write_object: bool) -> (FileId, String) {
        let (path, id) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let edit = CatalogEdit {
            add: vec![meta(FileTable::Spans, id, path.clone())],
            remove: Vec::new(),
            flushed_seq: None,
            sources: Vec::new(),
            raise: Vec::new(),
            quarantine: Vec::new(),
        };
        f.committer
            .stage(std::slice::from_ref(&path))
            .await
            .unwrap();
        f.committer.commit(&edit, |_| {}).await.unwrap();
        if write_object {
            f.objects
                .put_durable(&path, Bytes::from_static(SPANS_BYTES))
                .await
                .unwrap();
        }
        let remove = CatalogEdit {
            add: Vec::new(),
            remove: vec![(id, FileState::Active)],
            flushed_seq: None,
            sources: Vec::new(),
            raise: Vec::new(),
            quarantine: Vec::new(),
        };
        f.committer.commit(&remove, |_| {}).await.unwrap();
        (id, path)
    }

    async fn connect(dir: &Path) -> sqlx::SqliteConnection {
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap()
    }

    async fn scalar(dir: &Path, sql: &'static str) -> i64 {
        let mut connection = connect(dir).await;
        sqlx::query_scalar(sql)
            .fetch_one(&mut connection)
            .await
            .unwrap()
    }

    fn total_bytes() -> u64 {
        u64::try_from(SPANS_BYTES.len() + TRACES_BYTES.len()).unwrap()
    }

    #[tokio::test]
    async fn obsolete_files_are_collected_with_their_rows() {
        let f = fixture().await;
        let (_ids, (spans_path, traces_path)) = obsolete_pair(&f).await;

        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            ObsoleteReport {
                deleted: 2,
                bytes: total_bytes(),
                in_use: 0,
            }
        );
        assert!(!object_exists(f.dir.path(), &spans_path));
        assert!(!object_exists(f.dir.path(), &traces_path));
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM data_files").await,
            0
        );
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM file_sources").await,
            0
        );
    }

    #[tokio::test]
    async fn the_grace_keeps_freshly_obsolete_files() {
        let f = fixture().await;
        let (_ids, (spans_path, traces_path)) = obsolete_pair(&f).await;
        let obsoleted_at = scalar(
            f.dir.path(),
            "SELECT MIN(obsoleted_at) FROM data_files WHERE state = 'obsolete'",
        )
        .await;
        let grace = Duration::from_secs(300);
        let minute = 60_000_000_000_i64;

        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            obsoleted_at + minute,
            grace,
        )
        .await
        .unwrap();
        assert_eq!(report, ObsoleteReport::default());
        assert!(object_exists(f.dir.path(), &spans_path));
        assert!(object_exists(f.dir.path(), &traces_path));
        assert_eq!(
            scalar(
                f.dir.path(),
                "SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'"
            )
            .await,
            2
        );

        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            obsoleted_at + 5 * minute,
            grace,
        )
        .await
        .unwrap();
        assert_eq!(report.deleted, 2);
        assert!(!object_exists(f.dir.path(), &spans_path));
        assert!(!object_exists(f.dir.path(), &traces_path));
    }

    #[tokio::test]
    async fn an_older_catalog_keeps_its_files() {
        let f = fixture().await;
        let ((spans_id, traces_id), _paths) = add_pair(&f, true).await;
        // A catalog from before the removing commit still names both files.
        let held = f.committer.catalog().await;
        remove_pair(&f, spans_id, traces_id).await;

        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            ObsoleteReport {
                deleted: 0,
                bytes: 0,
                in_use: 2,
            }
        );

        drop(held);
        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(report.deleted, 2);
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM data_files").await,
            0
        );
    }

    #[tokio::test]
    async fn after_a_restart_the_grace_alone_decides() {
        let f = fixture().await;
        let (_ids, (spans_path, _traces_path)) = obsolete_pair(&f).await;

        let reopened = Committer::open(f.dir.path(), Hooks::default())
            .await
            .unwrap();
        let report = collect_obsolete(
            &reopened,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(report.deleted, 2);
        assert!(!object_exists(f.dir.path(), &spans_path));
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM data_files").await,
            0
        );
    }

    #[tokio::test]
    async fn an_obsolete_row_without_an_object_counts_zero_bytes() {
        let f = fixture().await;
        let (_id, path) = obsolete_one(&f, false).await;

        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            ObsoleteReport {
                deleted: 1,
                bytes: 0,
                in_use: 0,
            }
        );
        assert!(!object_exists(f.dir.path(), &path));
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM data_files").await,
            0
        );
    }

    #[tokio::test]
    async fn a_fault_after_the_object_delete_leaves_the_row() {
        let f = fixture().await;
        let (_id, path) = obsolete_one(&f, true).await;
        let hooks = Hooks::default();
        hooks.arm_fail(HookPoint::GcObsoleteAfterDelete);

        let error = collect_obsolete(&f.committer, &f.objects, &hooks, now_ns(), Duration::ZERO)
            .await
            .unwrap_err();
        assert!(matches!(error, GcError::Fault), "{error}");
        assert!(!object_exists(f.dir.path(), &path));
        assert_eq!(
            scalar(
                f.dir.path(),
                "SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'"
            )
            .await,
            1
        );

        // The object is gone; the next collection removes the row for zero bytes.
        let report = collect_obsolete(
            &f.committer,
            &f.objects,
            &Hooks::default(),
            now_ns(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(
            report,
            ObsoleteReport {
                deleted: 1,
                bytes: 0,
                in_use: 0,
            }
        );
        assert_eq!(
            scalar(f.dir.path(), "SELECT COUNT(*) FROM data_files").await,
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fresh_storage_collects_nothing_obsolete() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        assert_eq!(
            storage.collect_obsolete(Duration::ZERO).await.unwrap(),
            ObsoleteReport::default()
        );
        storage.shutdown().await.unwrap();
    }

    // ---- Orphan collection (N M2, D-N-21) ----

    const ORPHAN_BYTES: &[u8] = b"orphan-bytes";

    /// A spans key that no catalog row names.
    fn unknown_spans_key(day: &str) -> String {
        format!(
            "tables/spans/project={}/day={day}/0-{}.parquet",
            project(),
            uuid::Uuid::now_v7()
        )
    }

    async fn put_raw(storage: &crate::Storage, key: &str, bytes: &'static [u8]) {
        storage
            .objects
            .raw_store()
            .put(&ObjectPath::from(key), Bytes::from_static(bytes).into())
            .await
            .unwrap();
    }

    async fn delete_raw(storage: &crate::Storage, key: &str) {
        storage
            .objects
            .raw_store()
            .delete(&ObjectPath::from(key))
            .await
            .unwrap();
    }

    /// Every store key under `prefix`, as the store lists it.
    async fn listed(storage: &crate::Storage, prefix: &str) -> Vec<String> {
        storage
            .objects
            .raw_store()
            .list(Some(&ObjectPath::from(prefix)))
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await
            .unwrap()
    }

    async fn only_key(storage: &crate::Storage, prefix: &str) -> String {
        let keys = listed(storage, prefix).await;
        assert_eq!(keys.len(), 1, "{keys:?}");
        keys.into_iter().next().unwrap()
    }

    fn media_blob() -> MediaBlob {
        let bytes = Bytes::from_static(b"orphan-test-blob");
        MediaBlob {
            sha256: Sha256::digest(&bytes).into(),
            mime_type: "image/png".to_owned(),
            bytes,
        }
    }

    /// Writes the project's state into catalog.db from a second connection. A real
    /// drop deletes the media first, so this is the only way to keep a dropping
    /// project's media in place.
    async fn set_project_state(dir: &Path, state: &'static str) {
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .connect()
            .await
            .unwrap();
        sqlx::query(
            "INSERT OR REPLACE INTO projects (project_id, state, dropped_at) VALUES (?, ?, 0)",
        )
        .bind(project().to_string())
        .bind(state)
        .execute(&mut connection)
        .await
        .unwrap();
    }

    /// Runs [`crate::Maintenance::collect_orphans`] on a data directory whose server has
    /// stopped, as the admin commands do.
    async fn orphans(dir: &Path, min_age: Duration, dry_run: bool) -> OrphanReport {
        let maintenance = crate::Maintenance::open(config_in(dir)).await.unwrap();
        let report = maintenance.collect_orphans(min_age, dry_run).await.unwrap();
        maintenance.close().await.unwrap();
        report
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_deletes_unknown_old_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        let orphan = unknown_spans_key("2026-01-01");
        put_raw(&storage, &orphan, ORPHAN_BYTES).await;
        let spans: Vec<String> = storage
            .catalog_files(Table::Spans)
            .iter()
            .map(|file| file.path.clone())
            .collect();
        assert!(!spans.is_empty());
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(
            report,
            OrphanReport {
                orphans: 1,
                orphan_bytes: u64::try_from(ORPHAN_BYTES.len()).unwrap(),
                ..OrphanReport::default()
            }
        );
        assert!(!object_exists(dir.path(), &orphan));
        for path in spans {
            assert!(object_exists(dir.path(), &path));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_dry_run_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let orphan = unknown_spans_key("2026-01-01");
        put_raw(&storage, &orphan, ORPHAN_BYTES).await;
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, true).await;

        assert_eq!(
            report,
            OrphanReport {
                orphans: 1,
                orphan_bytes: u64::try_from(ORPHAN_BYTES.len()).unwrap(),
                dry_run: true,
                ..OrphanReport::default()
            }
        );
        assert!(object_exists(dir.path(), &orphan));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_leaves_recent_unknown_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let recent = unknown_spans_key("2026-01-01");
        put_raw(&storage, &recent, ORPHAN_BYTES).await;
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::from_secs(3_600), false).await;

        assert_eq!(
            report,
            OrphanReport {
                recent_unknown: 1,
                ..OrphanReport::default()
            }
        );
        assert!(object_exists(dir.path(), &recent));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_keeps_an_object_from_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let future = unknown_spans_key("2026-01-01");
        put_raw(&storage, &future, ORPHAN_BYTES).await;
        // A modification time an hour ahead, as a clock that ran fast would leave.
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("objects").join(&future))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(3_600))
            .unwrap();
        storage.shutdown().await.unwrap();

        // `min_age` is zero: only the future mtime keeps the object.
        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(
            report,
            OrphanReport {
                recent_unknown: 1,
                ..OrphanReport::default()
            }
        );
        assert!(object_exists(dir.path(), &future));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_reports_missing_needed_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        let path = storage.catalog_files(Table::Spans)[0].path.clone();
        delete_raw(&storage, &path).await;
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(
            report,
            OrphanReport {
                missing: 1,
                ..OrphanReport::default()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_keeps_staged_and_obsolete_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        let staged = unknown_spans_key("2026-01-02");
        put_raw(&storage, &staged, b"staged").await;
        storage
            .committer
            .stage(std::slice::from_ref(&staged))
            .await
            .unwrap();
        // Removing the files obsoletes them, as a compaction commit does.
        let files: Vec<Arc<FileMeta>> = storage
            .catalog_files(Table::Spans)
            .into_iter()
            .chain(storage.catalog_files(FileTable::Traces))
            .collect();
        assert!(!files.is_empty());
        let remove = CatalogEdit {
            add: Vec::new(),
            remove: files
                .iter()
                .map(|file| (file.id, FileState::Active))
                .collect(),
            flushed_seq: None,
            sources: Vec::new(),
            raise: Vec::new(),
            quarantine: Vec::new(),
        };
        storage.committer.commit(&remove, |_| {}).await.unwrap();
        assert_eq!(storage.staged_paths().await.unwrap(), vec![staged.clone()]);
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(report, OrphanReport::default());
        for file in &files {
            assert!(object_exists(dir.path(), &file.path));
        }
        assert!(object_exists(dir.path(), &staged));
        assert_eq!(
            scalar(
                dir.path(),
                "SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'"
            )
            .await,
            i64::try_from(files.len()).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_reports_missing_media() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        storage.put_media(project(), media_blob()).await.unwrap();
        let key = only_key(&storage, "media").await;
        delete_raw(&storage, &key).await;
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(
            report,
            OrphanReport {
                missing_media: 1,
                ..OrphanReport::default()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_keeps_dropping_project_media() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        storage.put_media(project(), media_blob()).await.unwrap();
        let key = only_key(&storage, "media").await;
        delete_raw(&storage, &key).await;
        set_project_state(dir.path(), "dropping").await;
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        // Known but not needed: the missing blob is not counted.
        assert_eq!(report, OrphanReport::default());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_orphans_skips_temp_names() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
        let temp = format!(
            "tables/spans/project={}/day=2026-01-01/.0-{}.parquet.tmp-{}",
            project(),
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7()
        );
        put_raw(&storage, &temp, ORPHAN_BYTES).await;
        // The store lists it, so the skip is what keeps it.
        assert!(listed(&storage, "tables").await.contains(&temp));
        storage.shutdown().await.unwrap();

        let report = orphans(dir.path(), Duration::ZERO, false).await;

        assert_eq!(report, OrphanReport::default());
        assert!(object_exists(dir.path(), &temp));
    }
}
