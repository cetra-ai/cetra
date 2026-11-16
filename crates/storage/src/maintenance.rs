//! Offline maintenance of a data directory (N M2, D-N-21, D-N-22).
//!
//! `cetra admin gc-orphans` and `cetra admin migrate` open the directory through
//! [`Maintenance`], not [`crate::Storage`]. It takes `LOCK` and migrates
//! `catalog.db` (with its pre-migration copy), and nothing else: no WAL replay, no
//! startup collection, no temporary-file or spill cleanup, no background task. A
//! `--dry-run` therefore changes neither the objects nor the catalog.

use crate::fs::File;
use std::sync::Arc;
use std::time::Duration;

use object_store::ObjectStore;

use crate::catalog::Committer;
use crate::config::StorageConfig;
use crate::error::{OpenError, OrphanError, StorageError};
use crate::gc::{self, OrphanReport};
use crate::hooks::Hooks;
use crate::objects::{Objects, PutSizes};

/// A data directory open for maintenance. Holds `LOCK` until [`Self::close`].
#[derive(Debug)]
pub struct Maintenance {
    lock: File,
    committer: Committer,
    objects: Objects,
}

impl Maintenance {
    /// Validates `config`, takes `data_dir/LOCK` and migrates `catalog.db`. Fails with
    /// [`OpenError::Locked`] while a server or another storage holds the directory.
    ///
    /// # Errors
    ///
    /// [`OpenError`] when the configuration, the lock, the catalog or the object store
    /// fails.
    pub async fn open(config: StorageConfig) -> Result<Self, OpenError> {
        Self::open_inner(config, None).await
    }

    /// [`Self::open`] with a remote store standing in for S3.
    #[cfg(test)]
    pub(crate) async fn open_with_store(
        config: StorageConfig,
        store: Arc<dyn ObjectStore>,
    ) -> Result<Self, OpenError> {
        Self::open_inner(config, Some((store, PutSizes::S3))).await
    }

    async fn open_inner(
        config: StorageConfig,
        remote: Option<(Arc<dyn ObjectStore>, PutSizes)>,
    ) -> Result<Self, OpenError> {
        // `gc-orphans` must delete even when the server's config is in restore mode
        // (D-N-34): the drill runs it with the same `cetra.toml`.
        let config = StorageConfig {
            delete_objects: true,
            ..config
        };
        #[allow(clippy::default_constructed_unit_structs)]
        let hooks = Hooks::default();
        let (lock, committer, objects) = crate::open_shared(&config, &hooks, remote).await?;
        Ok(Self {
            lock,
            committer,
            objects,
        })
    }

    /// Lists the object store under `tables/` and `media/`. Deletes the objects the
    /// catalog does not know that are older than `min_age` (unless `dry_run`), and
    /// counts the objects the catalog needs that are missing (N M2, D-N-21).
    ///
    /// Holds `object_gc_gate` for write while it runs.
    ///
    /// # Errors
    ///
    /// [`OrphanError`] when listing, deleting or reading the catalog fails.
    pub async fn collect_orphans(
        &self,
        min_age: Duration,
        dry_run: bool,
    ) -> Result<OrphanReport, OrphanError> {
        gc::collect_orphans(&self.committer, &self.objects, min_age, dry_run).await
    }

    /// Closes the catalog connection, then releases `LOCK`.
    ///
    /// # Errors
    ///
    /// [`StorageError::Catalog`] when SQLite fails to close. The lock is released
    /// either way.
    pub async fn close(self) -> Result<(), StorageError> {
        self.committer.close().await?;
        drop(self.lock);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use cetra_core::ids::ProjectId;
    use chrono::NaiveDate;
    use futures::TryStreamExt;
    use object_store::memory::InMemory;
    use object_store::prefix::PrefixStore;
    use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
    use sqlx::ConnectOptions;

    use super::Maintenance;
    use crate::Storage;
    use crate::catalog::FileTable;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::gc::OrphanReport;
    use crate::objects::Objects;
    use crate::wal::{Table, spans_at};

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn config_in(dir: &Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: bytesize::ByteSize::b(0),
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

    /// Every key of `store`, sorted.
    async fn keys(store: &Arc<dyn ObjectStore>) -> Vec<String> {
        let mut keys: Vec<String> = store
            .list(None)
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await
            .unwrap();
        keys.sort();
        keys
    }

    async fn put(store: &Arc<dyn ObjectStore>, key: &str, bytes: &'static [u8]) {
        store
            .put(&ObjectPath::from(key), Bytes::from_static(bytes).into())
            .await
            .unwrap();
    }

    /// Opens `dir` with `store` for maintenance, runs the orphan pass and closes.
    async fn maintenance_orphans(
        dir: &Path,
        store: &Arc<dyn ObjectStore>,
        min_age: Duration,
        dry_run: bool,
    ) -> OrphanReport {
        let maintenance = Maintenance::open_with_store(config_in(dir), Arc::clone(store))
            .await
            .unwrap();
        let report = maintenance.collect_orphans(min_age, dry_run).await.unwrap();
        maintenance.close().await.unwrap();
        report
    }

    /// A store wrapped the way the S3 store is: a bucket with a key prefix.
    fn prefixed_store() -> Arc<dyn ObjectStore> {
        Arc::new(PrefixStore::new(InMemory::new(), "backups/cetra"))
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dry_run_maintenance_open_changes_no_object_and_no_catalog_byte() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = Storage::open_with_store(
            config_in(dir.path()),
            Arc::clone(&store),
            crate::objects::PutSizes::S3,
        )
        .await
        .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        storage.shutdown().await.unwrap();

        // The state a stale restore can leave: a staged upload older than an hour,
        // not published; a file obsolete past any grace; and an unknown old object.
        let staged = Objects::table_file_path(FileTable::Spans, project(), day(), 1).0;
        let unknown = Objects::table_file_path(FileTable::Spans, project(), day(), 1).0;
        put(&store, &staged, b"staged").await;
        put(&store, &unknown, b"unknown").await;
        {
            let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.path().join("catalog.db"))
                .connect()
                .await
                .unwrap();
            sqlx::query("UPDATE data_files SET state = 'obsolete', obsoleted_at = 0")
                .execute(&mut connection)
                .await
                .unwrap();
            sqlx::query("INSERT INTO staged_objects (path, created_at) VALUES (?, 0)")
                .bind(&staged)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        // The first close checkpoints the WAL the setup wrote, so the comparison
        // below starts from a settled file.
        maintenance_orphans(dir.path(), &store, Duration::from_secs(3_600), true).await;
        let objects_before = keys(&store).await;
        let catalog_before = std::fs::read(dir.path().join("catalog.db")).unwrap();

        let report = maintenance_orphans(dir.path(), &store, Duration::ZERO, true).await;

        assert!(report.dry_run);
        assert_eq!(report.orphans, 1, "{report:?}");
        assert_eq!(keys(&store).await, objects_before);
        assert_eq!(
            std::fs::read(dir.path().join("catalog.db")).unwrap(),
            catalog_before
        );
    }

    /// Stores one spans file, backs the catalog up, and returns the backup and the
    /// store with the post-backup objects written. The live data dir is shut down.
    async fn backup_then_write(
        root: &Path,
        store: &Arc<dyn ObjectStore>,
    ) -> (std::path::PathBuf, Vec<String>, Vec<String>) {
        let live = root.join("live");
        let storage = Storage::open_with_store(
            config_in(&live),
            Arc::clone(store),
            crate::objects::PutSizes::S3,
        )
        .await
        .unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        storage.flush_now(Duration::from_secs(10)).await.unwrap();
        let published: Vec<String> = storage
            .catalog_files(Table::Spans)
            .iter()
            .map(|file| file.path.clone())
            .collect();
        assert!(!published.is_empty());
        let backup = root.join("snapshot.db");
        let snapshot = storage.snapshot_catalog(&backup).await.unwrap();
        drop(snapshot);
        let before = keys(store).await;
        // After the backup: an unknown data file and an unknown media blob.
        let data = Objects::table_file_path(FileTable::Spans, project(), day(), 0).0;
        let media = Objects::media_path(project(), &"ab".repeat(32));
        put(store, &data, b"post-backup data").await;
        put(store, &media, b"post-backup media").await;
        storage.shutdown().await.unwrap();
        (backup, published, before)
    }

    /// Copies the backup into a fresh data dir, as a restore does.
    fn restore(root: &Path, backup: &Path) -> std::path::PathBuf {
        let restored = root.join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        std::fs::copy(backup, restored.join("catalog.db")).unwrap();
        restored
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_restore_against_an_s3_style_store_deletes_exactly_the_post_backup_objects() {
        let root = tempfile::tempdir().unwrap();
        let store = prefixed_store();
        let (backup, _, before) = backup_then_write(root.path(), &store).await;
        let restored = restore(root.path(), &backup);

        let report = maintenance_orphans(&restored, &store, Duration::ZERO, false).await;

        assert_eq!(
            report,
            OrphanReport {
                orphans: 2,
                orphan_bytes: u64::try_from(b"post-backup data".len() + b"post-backup media".len())
                    .unwrap(),
                ..OrphanReport::default()
            }
        );
        assert_eq!(keys(&store).await, before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_restore_reports_a_file_the_backup_lists_that_the_store_lost() {
        let root = tempfile::tempdir().unwrap();
        let store = prefixed_store();
        let (backup, published, _) = backup_then_write(root.path(), &store).await;
        // Obsolete in the live catalog and collected from the store, but still active
        // in the backup.
        store
            .delete(&ObjectPath::from(published[0].as_str()))
            .await
            .unwrap();
        let restored = restore(root.path(), &backup);

        let report = maintenance_orphans(&restored, &store, Duration::ZERO, false).await;

        assert_eq!(report.missing, 1, "{report:?}");
        assert_eq!(report.missing_media, 0);
        assert_eq!(report.orphans, 2);
    }
}
