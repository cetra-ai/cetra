//! Consistent copies of `catalog.db` for online backups (N M2, D-N-18).
//!
//! [`crate::Storage::snapshot_catalog`] writes the copy and returns a
//! [`CatalogSnapshot`]. The snapshot pins every object it lists until it is
//! dropped: data files through the `retired` reference count, media through
//! the `object_gc_gate`.

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, MutexGuard, PoisonError};

use cetra_core::ids::ProjectId;
use sqlx::{ConnectOptions, Connection};
use tokio::sync::OwnedRwLockWriteGuard;

use crate::CopyCancel;
use crate::catalog::{Catalog, table_index};
use crate::error::{CopyObjectsError, SnapshotError};
use crate::fs;
use crate::objects::Objects;
use crate::wal::Table;

/// A consistent copy of `catalog.db` and the pin that keeps its objects (N M2,
/// D-N-18). Dropping it releases the pin.
pub struct CatalogSnapshot {
    /// The copied file.
    pub path: PathBuf,
    /// Every object the copy needs: active data files and live media.
    pub objects: Vec<LiveObject>,
    /// `table_state.flushed_seq` of the copy: `[spans, scores]`.
    pub flushed_seq: [u64; 2],
    /// Keeps the in-memory catalog of the copy alive, so its data files stay
    /// referenced through `retired`. Defence in depth: the gate below already
    /// blocks every deletion while the snapshot lives.
    _catalog: Arc<Catalog>,
    /// Blocks every object deletion (obsolete files, media, dropped-project
    /// media) while held.
    _gate: OwnedRwLockWriteGuard<()>,
}

impl CatalogSnapshot {
    /// Built by [`crate::Storage::snapshot_catalog`] once the copy is written
    /// and read, with the pins it must hold.
    pub(crate) fn new(
        path: PathBuf,
        objects: Vec<LiveObject>,
        flushed_seq: [u64; 2],
        catalog: Arc<Catalog>,
        gate: OwnedRwLockWriteGuard<()>,
    ) -> Self {
        Self {
            path,
            objects,
            flushed_seq,
            _catalog: catalog,
            _gate: gate,
        }
    }
}

impl fmt::Debug for CatalogSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogSnapshot")
            .field("path", &self.path)
            .field("objects", &self.objects.len())
            .field("flushed_seq", &self.flushed_seq)
            .finish_non_exhaustive()
    }
}

/// One object a snapshot needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveObject {
    /// Store-relative key: `tables/…/….parquet` or `media/{project}/{sha256}`.
    pub key: String,
    /// Size of the object in bytes, as the catalog records it.
    pub size_bytes: u64,
}

/// Reads the objects and the sequences of a copy written by `VACUUM INTO`.
///
/// Active data files come first in key order, then live media. Media of a
/// dropping or dropped project is not listed: its drop deletes it.
pub(crate) async fn read_copy(path: &Path) -> Result<(Vec<LiveObject>, [u64; 2]), SnapshotError> {
    let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await?;
    let files: Vec<(String, i64, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT path, size_bytes, pack_path, pack_size_bytes FROM data_files WHERE state = 'active'",
    )
    .fetch_all(&mut connection)
    .await?;
    let media: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT m.project_id, m.sha256, m.size_bytes FROM media AS m
         WHERE NOT EXISTS (
           SELECT 1 FROM projects AS p
           WHERE p.project_id = m.project_id AND p.state IN ('dropping', 'dropped'))
         ORDER BY m.project_id, m.sha256",
    )
    .fetch_all(&mut connection)
    .await?;
    let sequences: Vec<(String, i64)> =
        sqlx::query_as("SELECT table_name, flushed_seq FROM table_state")
            .fetch_all(&mut connection)
            .await?;
    connection.close().await?;

    let mut file_objects = Vec::with_capacity(files.len());
    for (key, size, pack_path, pack_size) in files {
        file_objects.push(LiveObject {
            key,
            size_bytes: stored_u64(size)?,
        });
        match (pack_path, pack_size) {
            (Some(pack_key), Some(pack_size)) => {
                if pack_key.is_empty() || pack_size <= 0 {
                    return Err(SnapshotError::Db(sqlx::Error::Decode(
                        "invalid pack metadata in the copy".into(),
                    )));
                }
                file_objects.push(LiveObject {
                    key: pack_key,
                    size_bytes: stored_u64(pack_size)?,
                });
            }
            (None, None) => {}
            _ => {
                return Err(SnapshotError::Db(sqlx::Error::Decode(
                    "incomplete pack metadata in the copy".into(),
                )));
            }
        }
    }
    file_objects.sort_by(|left, right| left.key.cmp(&right.key));
    let mut objects = Vec::with_capacity(file_objects.len() + media.len());
    objects.extend(file_objects);
    for (project, sha, size) in media {
        let project: ProjectId = project.parse().map_err(|_| SnapshotError::BadProjectId)?;
        objects.push(LiveObject {
            key: Objects::media_path(project, &sha),
            size_bytes: stored_u64(size)?,
        });
    }
    let mut flushed_seq = [0, 0];
    for (table, sequence) in sequences {
        let index = match table.as_str() {
            "spans" => table_index(Table::Spans),
            "scores" => table_index(Table::Scores),
            _ => {
                return Err(SnapshotError::Db(sqlx::Error::Decode(
                    "unknown table_name in the copy".into(),
                )));
            }
        };
        flushed_seq[index] = stored_u64(sequence)?;
    }
    Ok((objects, flushed_seq))
}

/// A stored integer that must not be negative.
fn stored_u64(value: i64) -> Result<u64, SnapshotError> {
    u64::try_from(value)
        .map_err(|_| SnapshotError::Db(sqlx::Error::Decode("negative value in the copy".into())))
}

/// What [`crate::Storage::copy_objects`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CopyReport {
    /// Objects placed under the destination.
    pub count: u64,
    /// Their total size in bytes.
    pub bytes: u64,
    /// Objects hard-linked.
    pub linked: u64,
    /// Objects copied because a hard link was impossible.
    pub copied: u64,
}

/// How one object reached the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Linked {
    Hard,
    Copied,
}

/// Places every object of `objects` under `dest` at its key, relative to the
/// local store root `root`. Runs on a blocking thread.
///
/// Each object's check of `cancel`, its directory creation and its link happen
/// under the lock, so a canceller that sets the flag under the lock and then
/// removes `dest` cannot race a directory or link back into it.
pub(crate) fn copy_all(
    root: &Path,
    dest: &Path,
    objects: &[LiveObject],
    cancel: &CopyCancel,
) -> Result<CopyReport, CopyObjectsError> {
    // Checked before anything is created: a key must stay under `root` and `dest`.
    for (key_index, object) in objects.iter().enumerate() {
        if !is_plain_relative(&object.key) {
            return Err(CopyObjectsError::Io {
                key_index,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "object key is not a plain relative path",
                ),
            });
        }
    }
    {
        let _held = not_cancelled(cancel)?;
        fs::create_dir_all(dest).map_err(|source| CopyObjectsError::Dir { source })?;
    }
    let mut report = CopyReport::default();
    for (key_index, object) in objects.iter().enumerate() {
        let io_error = |source| CopyObjectsError::Io { key_index, source };
        let source = root.join(&object.key);
        let target = dest.join(&object.key);
        let _held = not_cancelled(cancel)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        match link_or_copy(&source, &target, false).map_err(io_error)? {
            Linked::Hard => report.linked += 1,
            Linked::Copied => report.copied += 1,
        }
        report.count += 1;
        report.bytes += object.size_bytes;
    }
    let _held = not_cancelled(cancel)?;
    sync_dirs(dest).map_err(|source| CopyObjectsError::Dir { source })?;
    Ok(report)
}

/// Locks the cancel flag and returns the guard, or [`CopyObjectsError::Cancelled`]
/// when it is set. The guard must be held across the filesystem calls it covers.
fn not_cancelled(cancel: &CopyCancel) -> Result<MutexGuard<'_, bool>, CopyObjectsError> {
    let guard = cancel.lock().unwrap_or_else(PoisonError::into_inner);
    if *guard {
        Err(CopyObjectsError::Cancelled)
    } else {
        Ok(guard)
    }
}

/// Whether `key` names a file strictly below the store root: non-empty, made of
/// normal components only (no `..`, root, prefix or `.`).
fn is_plain_relative(key: &str) -> bool {
    !key.is_empty()
        && Path::new(key)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Hard-links `source` to `target`, or copies it and syncs the copy when the
/// link is refused or `force_copy` is set. Filesystems refuse links for many
/// reasons (`EXDEV`, `EMLINK`, `EPERM`, `ENOTSUP`), so every link error falls
/// back to a copy except `NotFound` (no source) and `AlreadyExists` (occupied
/// target), which the copy would hit as well. Errors of the copy are returned.
fn link_or_copy(source: &Path, target: &Path, force_copy: bool) -> io::Result<Linked> {
    if !force_copy {
        match fs::hard_link(source, target) {
            Ok(()) => return Ok(Linked::Hard),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
                ) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    fs::copy(source, target)?;
    fs::OpenOptions::new().read(true).open(target)?.sync_all()?;
    Ok(Linked::Copied)
}

/// Makes every directory under `dir` durable, `dir` included.
fn sync_dirs(dir: &Path) -> io::Result<()> {
    fs::sync_dir(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_dirs(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use sha2::{Digest, Sha256};
    use sqlx::ConnectOptions;

    use super::{Linked, LiveObject, link_or_copy};
    use crate::CopyCancel;
    use crate::QueryOptions;
    use crate::Storage;
    use crate::catalog::{FileTable, now_ns};
    use crate::compaction::job::tests::{at, config_in, flush_cycle, project, quiet_compaction};
    use crate::config::{CompactionConfig, ObjectStoreUrl};
    use crate::error::{AppendError, CopyObjectsError, SnapshotError};
    use crate::media::MediaBlob;
    use crate::objects::Objects;
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::wal::{Table, spans_at};
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn blob_of(bytes: &'static [u8]) -> MediaBlob {
        let bytes = Bytes::from_static(bytes);
        MediaBlob {
            sha256: Sha256::digest(bytes.as_ref()).into(),
            mime_type: "image/png".to_owned(),
            bytes,
            newest_span_start: 0,
        }
    }

    /// Waits until the spans table has `count` active files and no memtable rows.
    async fn wait_for_spans_files(storage: &Storage, count: usize) {
        let started = Instant::now();
        while storage.catalog_files(Table::Spans).len() < count
            || storage.published_rows(Table::Spans) != 0
        {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "flush did not publish the spans"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn object_on_disk(dir: &std::path::Path, key: &str) -> bool {
        dir.join("objects").join(key).is_file()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_lists_active_files_and_media() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_millis(100));
        let storage = Storage::open(config).await.unwrap();
        let bytes = b"snapshot-media";
        let blob = blob_of(bytes);
        let media_key = Objects::media_path(project(), &hex::encode(blob.sha256));
        storage.put_media(project(), blob).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1, 2]))
            .await
            .unwrap();
        wait_for_spans_files(&storage, 1).await;

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();

        let mut expected = storage.catalog_paths(FileTable::Spans);
        expected.extend(storage.catalog_paths(FileTable::Traces));
        expected.extend(storage.catalog_paths(FileTable::Scores));
        expected.sort();
        let mut listed: Vec<String> = snapshot
            .objects
            .iter()
            .map(|object| object.key.clone())
            .filter(|key| key.starts_with("tables/"))
            .collect();
        listed.sort();
        assert_eq!(listed, expected);
        assert!(
            snapshot.objects.iter().any(|object| object.key == media_key
                && object.size_bytes == u64::try_from(bytes.len()).unwrap()),
            "the live media is not listed"
        );
        assert_eq!(
            snapshot.flushed_seq[0],
            storage.committer.flushed_seq(Table::Spans).await
        );
        assert!(snapshot.flushed_seq[0] > 0);
        assert_eq!(snapshot.flushed_seq[1], 0);

        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&target)
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM data_files WHERE state = 'active'")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(usize::try_from(active).unwrap(), expected.len());
        drop(connection);

        drop(snapshot);
        storage.shutdown().await.unwrap();
    }

    /// A snapshot holds back the deletion of obsolete files: the GC waits for
    /// the gate while the snapshot is alive, then deletes the files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_holds_obsolete_gc() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(3), Duration::from_secs(3_600));
        let mut storage = Storage::open(config.clone()).await.unwrap();
        for index in 0..3_u8 {
            let base = at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
            let batch = span_rows(&[
                span_row(0, 10 + index, base, base + 1_000_000_000),
                span_row(1, 20 + index, base + 2_000_000_000, base + 3_000_000_000),
            ]);
            storage = flush_cycle(&config, storage, batch).await;
        }
        // The six inputs of the compaction below: three spans and three traces files.
        let mut obsoleted = storage.catalog_paths(FileTable::Spans);
        obsoleted.extend(storage.catalog_paths(FileTable::Traces));
        assert_eq!(obsoleted.len(), 6, "{obsoleted:?}");
        let storage = Arc::new(storage);

        let report = storage.compact(at(11, 0, 0)).await.unwrap();
        assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        for object in &snapshot.objects {
            assert!(
                !obsoleted.contains(&object.key),
                "{} is obsolete yet listed",
                object.key
            );
        }

        let collector = Arc::clone(&storage);
        let task = tokio::spawn(async move { collector.collect_obsolete(Duration::ZERO).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !task.is_finished(),
            "obsolete collection ran while a snapshot was held"
        );
        for path in &obsoleted {
            assert!(object_on_disk(dir.path(), path), "{path} was deleted");
        }
        for object in &snapshot.objects {
            assert!(
                object_on_disk(dir.path(), &object.key),
                "{} was deleted under a snapshot",
                object.key
            );
        }

        drop(snapshot);
        let report = task.await.unwrap().unwrap();
        assert_eq!(report.deleted, 6, "{report:?}");
        for path in &obsoleted {
            assert!(!object_on_disk(dir.path(), path), "{path} survived");
        }
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// While a snapshot holds the gate, a project drop commits and returns at
    /// once, and leaves its media for the GC task. Once the snapshot is gone,
    /// the GC step deletes the media.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_holds_media_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config).await.unwrap();
        let blob = blob_of(b"held-media");
        let key = Objects::media_path(project(), &hex::encode(blob.sha256));
        storage.put_media(project(), blob).await.unwrap();

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        assert!(snapshot.objects.iter().any(|object| object.key == key));

        tokio::time::timeout(Duration::from_secs(5), storage.drop_project(project()))
            .await
            .expect("the drop waited for the snapshot")
            .unwrap();
        assert!(
            storage
                .committer
                .dropping_projects()
                .await
                .unwrap()
                .contains(&project()),
            "the drop did not commit"
        );
        assert!(
            object_on_disk(dir.path(), &key),
            "the media was deleted under a snapshot"
        );

        drop(snapshot);
        storage.media.finish_drops().await;
        assert!(!object_on_disk(dir.path(), &key), "the media survived");
        assert!(
            storage
                .committer
                .dropping_projects()
                .await
                .unwrap()
                .is_empty(),
            "the drop did not finish"
        );
        storage.shutdown().await.unwrap();
    }

    /// Appends succeed and a flush driven by `flush.max_age` still commits
    /// while a snapshot is held.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_does_not_block_appends_or_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_millis(100));
        let storage = Storage::open(config).await.unwrap();
        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            storage.append_spans(project(), spans_at(&[1, 2])),
        )
        .await
        .expect("an append waited for the snapshot")
        .unwrap();
        wait_for_spans_files(&storage, 1).await;

        drop(snapshot);
        storage.shutdown().await.unwrap();
    }

    /// Writes one media blob and two spans, and flushes the spans.
    async fn flush_spans_and_media(storage: &Storage) {
        storage
            .put_media(project(), blob_of(b"copied-media"))
            .await
            .unwrap();
        storage
            .append_spans(project(), spans_at(&[1, 2]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_objects_links_every_object() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(
            dir.path(),
            quiet_compaction(8),
            Duration::from_secs(3_600),
        ))
        .await
        .unwrap();
        flush_spans_and_media(&storage).await;
        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        let dest = dir.path().join("backup-objects");

        let report = storage
            .copy_objects(&snapshot, &dest, not_cancelled())
            .await
            .unwrap();
        let count = u64::try_from(snapshot.objects.len()).unwrap();
        assert!(count >= 2, "{snapshot:?}");
        assert_eq!(report.count, count);
        assert_eq!(report.linked, count);
        assert_eq!(report.copied, 0);
        for object in &snapshot.objects {
            let copied = dest.join(&object.key);
            let metadata = std::fs::metadata(&copied).unwrap();
            assert_eq!(metadata.len(), object.size_bytes, "{}", object.key);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert!(metadata.nlink() >= 2, "{} was not linked", object.key);
            }
        }

        drop(snapshot);
        storage.shutdown().await.unwrap();
    }

    /// A cancel flag that is not set.
    fn not_cancelled() -> CopyCancel {
        Arc::new(Mutex::new(false))
    }

    #[test]
    fn copy_all_places_nothing_once_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("tables")).unwrap();
        std::fs::write(root.join("tables/a.parquet"), b"object bytes").unwrap();
        let dest = dir.path().join("dest");
        let objects = [LiveObject {
            key: "tables/a.parquet".into(),
            size_bytes: 12,
        }];
        let cancel: CopyCancel = Arc::new(Mutex::new(true));

        let result = super::copy_all(&root, &dest, &objects, &cancel);
        assert!(
            matches!(result, Err(CopyObjectsError::Cancelled)),
            "{result:?}"
        );
        assert!(!dest.exists(), "a cancelled copy created the destination");
    }

    #[test]
    fn link_or_copy_copies_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.parquet");
        std::fs::write(&source, b"object bytes").unwrap();

        let linked = dir.path().join("linked.parquet");
        assert_eq!(link_or_copy(&source, &linked, false).unwrap(), Linked::Hard);

        let copied = dir.path().join("copied.parquet");
        assert_eq!(
            link_or_copy(&source, &copied, true).unwrap(),
            Linked::Copied
        );
        assert_eq!(std::fs::read(&copied).unwrap(), b"object bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&copied).unwrap().nlink(), 1);
        }
    }

    #[test]
    fn copy_all_rejects_a_key_escaping_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("x"), b"outside the root").unwrap();
        let dest = dir.path().join("dest");
        let objects = [LiveObject {
            key: "../x".into(),
            size_bytes: 1,
        }];

        let result = super::copy_all(&root, &dest, &objects, &not_cancelled());
        assert!(
            matches!(
                result,
                Err(CopyObjectsError::Io { key_index: 0, ref source })
                    if source.kind() == std::io::ErrorKind::InvalidInput
            ),
            "{result:?}"
        );
        assert!(!dest.exists(), "a rejected key created the destination");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_objects_reports_an_unlinkable_object() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(
            dir.path(),
            quiet_compaction(8),
            Duration::from_secs(3_600),
        ))
        .await
        .unwrap();
        flush_spans_and_media(&storage).await;
        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        let dest = dir.path().join("backup-objects");
        // A directory where the first object must go: the link cannot be made.
        std::fs::create_dir_all(dest.join(&snapshot.objects[0].key)).unwrap();

        let result = storage
            .copy_objects(&snapshot, &dest, not_cancelled())
            .await;
        assert!(
            matches!(result, Err(CopyObjectsError::Io { key_index: 0, .. })),
            "{result:?}"
        );

        drop(snapshot);
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn copy_objects_refuses_a_remote_store() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open_with_store(
            config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600)),
            Arc::new(InMemory::new()),
            crate::objects::PutSizes::S3,
        )
        .await
        .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        let dest = dir.path().join("backup-objects");

        let result = storage
            .copy_objects(&snapshot, &dest, not_cancelled())
            .await;
        assert!(
            matches!(result, Err(CopyObjectsError::RemoteStore)),
            "{result:?}"
        );
        assert!(!dest.exists(), "a refused copy created the destination");

        drop(snapshot);
        storage.shutdown().await.unwrap();
    }

    /// Rows of the spans table that `project` sees through a query.
    async fn span_count(storage: &Storage) -> i64 {
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let ctx = crate::views::QueryContext::new(query, None).unwrap();
        let counted = crate::test_support::rows(&ctx, "SELECT count(*) AS n FROM spans").await;
        counted[0]["n"].as_i64().unwrap()
    }

    /// Rows of the scores table that `project` sees through a query.
    async fn score_count(storage: &Storage) -> i64 {
        let query = storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap();
        let ctx = crate::views::QueryContext::new(query, None).unwrap();
        let counted = crate::test_support::rows(&ctx, "SELECT count(*) AS n FROM scores").await;
        counted[0]["n"].as_i64().unwrap()
    }

    /// The core restore test: a backup taken from a running storage opens on
    /// a new data directory and answers the same queries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_copied_backup_opens_and_answers_queries() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(
            &dir.path().join("source"),
            quiet_compaction(8),
            Duration::from_secs(3_600),
        ))
        .await
        .unwrap();
        storage
            .append_spans(project(), spans_at(&[1, 2, 3]))
            .await
            .unwrap();
        let mut score = score_row(1, 1, 5, 6, 7, false, "accuracy");
        score.project_id = project();
        storage
            .append_scores(project(), score_rows(&[score]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();

        let backup = dir.path().join("backup");
        std::fs::create_dir(&backup).unwrap();
        let snapshot = storage
            .snapshot_catalog(&backup.join("catalog.db"))
            .await
            .unwrap();
        storage
            .copy_objects(&snapshot, &backup.join("objects"), not_cancelled())
            .await
            .unwrap();
        let flushed = snapshot.flushed_seq;
        drop(snapshot);
        storage.shutdown().await.unwrap();

        let restored_dir = dir.path().join("restored");
        std::fs::create_dir(&restored_dir).unwrap();
        std::fs::copy(backup.join("catalog.db"), restored_dir.join("catalog.db")).unwrap();
        let mut config = config_in(
            &restored_dir,
            quiet_compaction(8),
            Duration::from_secs(3_600),
        );
        config.object_store = Some(ObjectStoreUrl::Local(backup.join("objects")));
        let restored = Storage::open(config).await.unwrap();

        // Checked before any append: later flushes write into the backup's objects.
        assert_eq!(span_count(&restored).await, 3);
        assert_eq!(score_count(&restored).await, 1);

        let receipt = restored
            .append_spans(project(), spans_at(&[9]))
            .await
            .unwrap();
        assert!(
            receipt.seq > flushed[0],
            "the restored WAL reuses sequence {}",
            receipt.seq
        );
        restored.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_refuses_an_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config).await.unwrap();
        let target = dir.path().join("catalog-copy.db");
        let held = storage.snapshot_catalog(&target).await.unwrap();

        let again = tokio::time::timeout(Duration::from_secs(5), storage.snapshot_catalog(&target))
            .await
            .expect("a refused snapshot waited for the gate");
        assert!(
            matches!(again, Err(SnapshotError::Exists)),
            "the second snapshot was not refused"
        );

        drop(held);
        storage.shutdown().await.unwrap();
    }

    /// The copy keeps the format marker, so a restored directory is stamped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_snapshot_keeps_the_format_marker() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config).await.unwrap();
        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        drop(snapshot);
        storage.shutdown().await.unwrap();

        let mut copy = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&target)
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let marker: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut copy)
            .await
            .unwrap();
        assert_eq!(marker, i64::from(crate::format::FORMAT_VERSION));
    }

    /// The gate holds media collection while a snapshot lives: the collection
    /// waits, and the blob stays until the snapshot is dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_gate_holds_media_collection() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Arc::new(Storage::open(config).await.unwrap());
        let blob = blob_of(b"unreferenced-media");
        let key = Objects::media_path(project(), &hex::encode(blob.sha256));
        storage.put_media(project(), blob).await.unwrap();

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        // A year on: the blob's `last_seen_at` is past the grace period.
        let later = now_ns() + 365 * 24 * 3_600 * 1_000_000_000;
        let collector = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.collect_media(later).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !collector.is_finished(),
            "media collection ran while a snapshot was held"
        );
        assert!(object_on_disk(dir.path(), &key), "the blob was deleted");

        drop(snapshot);
        let report = collector.await.unwrap().unwrap();
        assert_eq!(report.deleted, 1, "{report:?}");
        assert!(!object_on_disk(dir.path(), &key), "the blob survived");
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// A quarantined file leaves the snapshot, and a drop then retires it: it
    /// becomes an obsolete row that is never in `retired`, so only the gate
    /// protects it during a backup. Every object the snapshot lists still
    /// exists while the snapshot lives, through a verify pass, a drop and an
    /// obsolete collection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_gate_holds_a_quarantined_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Storage::open(config.clone()).await.unwrap();
        let storage = flush_cycle(&config, storage, spans_at(&[1, 2])).await;
        let victim = storage.catalog_files(Table::Spans)[0].clone();
        let (victim_id, victim_path) = (victim.id, victim.path.clone());
        // Truncate the object: its size no longer matches the catalog.
        let object = dir.path().join("objects").join(&victim_path);
        let bytes = std::fs::read(&object).unwrap();
        std::fs::write(&object, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(storage.verify_queue().suspect([victim_id], now_ns()), 1);

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        let report = storage.verify_pass().await;
        assert_eq!(report.quarantined, 1, "{report:?}");
        // The drop retires the quarantined file (obsolete row, never in `retired`).
        storage.drop_project(project()).await.unwrap();

        let storage = Arc::new(storage);
        let collector = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.collect_obsolete(Duration::ZERO).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        for object in &snapshot.objects {
            assert!(
                object_on_disk(dir.path(), &object.key),
                "{} was deleted under a snapshot",
                object.key
            );
        }

        drop(snapshot);
        collector.await.unwrap().unwrap();
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// A copy taken while commits and compactions run lists exactly the active
    /// files the snapshot holds, and every object it lists exists while the
    /// snapshot lives. The listed files stay in use through the snapshot's
    /// retired references, so this test does not isolate the gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_snapshot_matches_its_copy_under_concurrent_commits() {
        let dir = tempfile::tempdir().unwrap();
        let compaction = CompactionConfig {
            interval: Duration::from_millis(100),
            l0_files: 2,
            ..CompactionConfig::default()
        };
        let config = config_in(dir.path(), compaction, Duration::from_millis(100));
        let storage = Arc::new(Storage::open(config).await.unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let appender = {
            let storage = Arc::clone(&storage);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut start = 0_i64;
                while !stop.load(Ordering::SeqCst) {
                    storage
                        .append_spans(project(), spans_at(&[start]))
                        .await
                        .unwrap();
                    start += 1;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };

        // Obsolete-file GC runs too: a file the snapshot lists that a later
        // compaction obsoletes must stay on disk while the snapshot lives
        // (kept by the retired references, not the gate).
        let collecting = {
            let storage = Arc::clone(&storage);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    storage.collect_obsolete(Duration::ZERO).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };

        for round in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let target = dir.path().join(format!("copy-{round}.db"));
            let snapshot = storage.snapshot_catalog(&target).await.unwrap();

            let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&target)
                .read_only(true)
                .connect()
                .await
                .unwrap();
            let mut copied: Vec<String> =
                sqlx::query_scalar("SELECT path FROM data_files WHERE state = 'active'")
                    .fetch_all(&mut connection)
                    .await
                    .unwrap();
            drop(connection);
            copied.sort();
            let mut listed: Vec<String> = snapshot
                .objects
                .iter()
                .map(|object| object.key.clone())
                .filter(|key| key.starts_with("tables/"))
                .collect();
            listed.sort();
            assert_eq!(
                copied, listed,
                "round {round}: the copy and the snapshot differ"
            );
            for object in &snapshot.objects {
                assert!(
                    object_on_disk(dir.path(), &object.key),
                    "round {round}: {} is gone",
                    object.key
                );
            }
            assert!(
                snapshot.flushed_seq[0] <= storage.committer.flushed_seq(Table::Spans).await,
                "round {round}: the copy is ahead of the committer"
            );
            drop(snapshot);
        }

        stop.store(true, Ordering::SeqCst);
        appender.await.unwrap();
        collecting.await.unwrap();
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    /// A drop during a snapshot returns within a second and refuses appends;
    /// its media stays until the snapshot is gone, then the GC step deletes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drop_project_during_a_snapshot_returns_and_finishes_later() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), quiet_compaction(8), Duration::from_secs(3_600));
        let storage = Arc::new(Storage::open(config).await.unwrap());
        let blob = blob_of(b"held-by-the-snapshot");
        let key = Objects::media_path(project(), &hex::encode(blob.sha256));
        storage.put_media(project(), blob).await.unwrap();

        let target = dir.path().join("catalog-copy.db");
        let snapshot = storage.snapshot_catalog(&target).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), storage.drop_project(project()))
            .await
            .expect("the drop waited for the snapshot")
            .unwrap();
        let refused = storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap_err();
        assert_eq!(refused, AppendError::ProjectDropped);
        assert!(object_on_disk(dir.path(), &key), "the media was deleted");

        // The GC step waits for the gate while the snapshot lives.
        let finishing = {
            let storage = Arc::clone(&storage);
            tokio::spawn(async move { storage.media.finish_drops().await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !finishing.is_finished(),
            "the drop finished while a snapshot was held"
        );
        assert!(object_on_disk(dir.path(), &key), "the media was deleted");

        drop(snapshot);
        finishing.await.unwrap();
        assert!(!object_on_disk(dir.path(), &key), "the media survived");
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn read_copy_emits_sorted_pack_live_objects_and_rejects_corrupt_pack_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let committer = crate::catalog::Committer::open(dir.path(), Default::default())
            .await
            .unwrap();
        let p = project();
        let d = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let spans_id = crate::catalog::FileId::new_v7();
        let traces_id = crate::catalog::FileId::new_v7();
        let pack_id = [0x42_u8; 16];
        // Choose keys where pack_path sorts strictly BEFORE spans_path and traces_path sorts last.
        let pack_path = format!("tables/spans/project={p}/day=2026-06-01/0-aaa-pack.stpk");
        let spans_path = format!("tables/spans/project={p}/day=2026-06-01/1-zzz-spans.parquet");
        let traces_path = format!("tables/traces/project={p}/day=2026-06-01/1-traces.parquet");
        let spans = crate::catalog::FileMeta {
            id: spans_id,
            table: FileTable::Spans,
            project: p,
            day: d,
            level: 1,
            path: spans_path.clone(),
            size: 120,
            footer_size: 16,
            rows: 2,
            min_time: 1,
            max_time: 2,
            min_ingest_time: 1,
            max_ingest_time: 2,
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: Some(crate::catalog::PackMeta {
                id: pack_id,
                path: pack_path.clone(),
                size: 345,
                toc_size: 112,
            }),
        };
        let traces = crate::catalog::FileMeta {
            id: traces_id,
            table: FileTable::Traces,
            project: p,
            day: d,
            level: 1,
            path: traces_path.clone(),
            size: 80,
            footer_size: 16,
            rows: 1,
            min_time: 1,
            max_time: 2,
            min_ingest_time: 1,
            max_ingest_time: 2,
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: None,
        };
        committer
            .stage(&[spans_path.clone(), pack_path.clone(), traces_path.clone()])
            .await
            .unwrap();
        committer
            .commit(
                &crate::catalog::CatalogEdit {
                    add: vec![spans, traces],
                    sources: vec![(traces_id, spans_id)],
                    ..crate::catalog::CatalogEdit::default()
                },
                |_| {},
            )
            .await
            .unwrap();
        let sha = "a".repeat(64);
        let media_key = Objects::media_path(p, &sha);
        committer
            .insert_media(p, &sha, "image/png", 99, &media_key, 10, 10)
            .await
            .unwrap();

        let copy = dir.path().join("snapshot.db");
        committer
            .snapshot_into(copy.to_str().unwrap())
            .await
            .unwrap();
        let (objects, _) = super::read_copy(&copy).await.unwrap();
        assert_eq!(
            objects,
            vec![
                LiveObject {
                    key: pack_path,
                    size_bytes: 345,
                },
                LiveObject {
                    key: spans_path,
                    size_bytes: 120,
                },
                LiveObject {
                    key: traces_path,
                    size_bytes: 80,
                },
                LiveObject {
                    key: media_key,
                    size_bytes: 99,
                },
            ]
        );

        // Negative pack_size_bytes in the copy must fail read_copy.
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&copy)
            .connect()
            .await
            .unwrap();
        sqlx::query("UPDATE data_files SET pack_size_bytes = -1 WHERE id = ?")
            .bind(spans_id.to_string())
            .execute(&mut conn)
            .await
            .unwrap();
        assert!(matches!(
            super::read_copy(&copy).await,
            Err(SnapshotError::Db(_))
        ));

        // Partial pack columns (pack_path present, pack_size_bytes NULL) must fail read_copy.
        sqlx::query("UPDATE data_files SET pack_size_bytes = NULL WHERE id = ?")
            .bind(spans_id.to_string())
            .execute(&mut conn)
            .await
            .unwrap();
        assert!(matches!(
            super::read_copy(&copy).await,
            Err(SnapshotError::Db(_))
        ));
    }
}
