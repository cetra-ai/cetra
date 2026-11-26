//! Where flushed Parquet files are stored, and how a local write becomes durable.
//!
//! `LocalFileSystem` in object_store 0.13 does not fsync. It is registered for
//! reads only. Local `put_durable` writes a temp file, `sync_all`s it, renames
//! it into place, and fsyncs the directory. Remote writes go through the store.
//!
//! `Storage::open` builds one [`Objects`] and removes leftover local temp files.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use cetra_core::ids::ProjectId;
use chrono::NaiveDate;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::prefix::PrefixStore;
use object_store::{
    GetOptions, GetRange, MultipartUpload, ObjectStore, ObjectStoreExt, PutPayload,
};
use uuid::Uuid;

use crate::catalog::{FileId, FileTable};
use crate::config::{ObjectStoreUrl, S3Config, StorageConfig};
use crate::error::{ObjectError, ProbeError};
use crate::fs::{self, OpenOptions};
use crate::hooks::{HookPoint, Hooks};
use crate::read_cache::{CacheStore, ReadCache, cached_name};
use tokio::io::AsyncReadExt;

/// The marker [`Objects::probe`] HEADs and, if that fails, writes (D-N-36). Outside
/// `tables/` and `media/`, so GC, orphan collection and backups never see it.
const PROBE_KEY: &str = "_probe/readyz";

/// The kind of a probe's store error, without its text (which holds the URL).
fn probe_kind(error: &object_store::Error) -> &'static str {
    match error {
        object_store::Error::NotFound { .. } => "not_found",
        object_store::Error::PermissionDenied { .. } => "permission_denied",
        _ => "other",
    }
}

/// When an upload switches to multipart, and the part size (spec 03 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PutSizes {
    /// `put` up to this size, inclusive; multipart above.
    pub(crate) single_max: usize,
    /// Size of every part except the last.
    pub(crate) part: usize,
}

impl PutSizes {
    /// What S3 gets: 64 MiB single puts, 16 MiB parts (S3's minimum part is 5 MiB).
    pub(crate) const S3: PutSizes = PutSizes {
        single_max: 64 * 1024 * 1024,
        part: 16 * 1024 * 1024,
    };
}

/// Object storage for one opened data directory.
#[derive(Debug)]
pub(crate) struct Objects {
    kind: Kind,
    /// The store itself: S3, or a test store standing in for it.
    raw: Arc<dyn ObjectStore>,
    /// `raw` behind the read cache (spec 03 §6), or `raw` itself. Used by
    /// queries, compaction and GC so reads go through the cache.
    store: Arc<dyn ObjectStore>,
    /// Set by [`Self::with_read_cache`], remote stores only.
    cache: Option<Arc<ReadCache>>,
    /// `<data_dir>/tmp/compaction`, where remote outputs spill before upload.
    spill_dir: PathBuf,
    hooks: Hooks,
    /// Set by the first failed local fsync. Local writes and deletes check it
    /// first (D-R-6). Never set in remote mode.
    sync_failed: Arc<AtomicBool>,
    /// The multipart thresholds of remote uploads. [`PutSizes::S3`] outside tests.
    put_sizes: PutSizes,
    /// `false` in restore mode (D-N-34): every deletion is refused with
    /// [`ObjectError::DeletionOff`]. Writes are not affected.
    delete_objects: bool,
}

#[derive(Debug)]
enum Kind {
    /// `root` is the canonical directory. Writes do not go through `store`.
    Local { root: PathBuf },
    /// S3, or a test store standing in for it.
    Remote,
}

impl Objects {
    /// Opens the store from `config`.
    ///
    /// Local roots are created, including missing parents. The parent of every
    /// directory of the chain is fsynced, new or not (see `create_dir_chain`).
    /// Blocking work runs on `spawn_blocking`.
    pub(crate) async fn new(config: &StorageConfig, hooks: Hooks) -> Result<Self, ObjectError> {
        let spill_dir = config.data_dir.join("tmp").join("compaction");
        match config.object_store_url() {
            ObjectStoreUrl::Local(root) => {
                let error_path = root.clone();
                let data_dir = config.data_dir.clone();
                let prepared =
                    tokio::task::spawn_blocking(move || prepare_local_root(&root, &data_dir)).await;
                let (root, store) = prepared.map_err(|source| {
                    ObjectError::io(
                        "opening object store",
                        &error_path,
                        io::Error::other(source),
                    )
                })??;
                let store: Arc<dyn ObjectStore> = Arc::new(store);
                Ok(Self {
                    kind: Kind::Local { root },
                    raw: Arc::clone(&store),
                    store,
                    cache: None,
                    spill_dir,
                    hooks,
                    sync_failed: Arc::new(AtomicBool::new(false)),
                    put_sizes: PutSizes::S3,
                    delete_objects: config.delete_objects,
                })
            }
            ObjectStoreUrl::S3 { bucket, prefix } => {
                let store = build_s3(&bucket, &prefix, &config.s3)?;
                Ok(Self {
                    kind: Kind::Remote,
                    raw: Arc::clone(&store),
                    store,
                    cache: None,
                    spill_dir,
                    hooks,
                    sync_failed: Arc::new(AtomicBool::new(false)),
                    put_sizes: PutSizes::S3,
                    delete_objects: config.delete_objects,
                })
            }
        }
    }

    /// Whether a local fsync failed in this process (D-R-6). Local writes and
    /// deletes are refused from then until a restart.
    pub(crate) fn sync_failed(&self) -> bool {
        self.sync_failed.load(Ordering::SeqCst)
    }

    /// Refuses a write or delete after a failed local fsync (D-R-6). Remote
    /// stores never set the flag, so they are not affected.
    fn refuse_after_sync_failure(&self) -> Result<(), ObjectError> {
        if self.sync_failed() {
            Err(ObjectError::SyncFailed)
        } else {
            Ok(())
        }
    }

    /// The local object root, `None` for a remote store.
    pub(crate) fn local_root(&self) -> Option<&Path> {
        match &self.kind {
            Kind::Local { root } => Some(root),
            Kind::Remote => None,
        }
    }

    /// Remote store for tests. `store` is usually [`crate::test_support::ChaosStore`].
    #[cfg(test)]
    pub(crate) fn remote_for_test(
        store: Arc<dyn ObjectStore>,
        data_dir: &Path,
        hooks: Hooks,
        put_sizes: PutSizes,
    ) -> Result<Self, ObjectError> {
        Ok(Self {
            kind: Kind::Remote,
            raw: Arc::clone(&store),
            store,
            cache: None,
            spill_dir: data_dir.join("tmp").join("compaction"),
            hooks,
            sync_failed: Arc::new(AtomicBool::new(false)),
            put_sizes,
            delete_objects: true,
        })
    }

    /// Puts `cache` in front of the remote store (spec 03 §6). Data-file reads
    /// go through it; writes and everything else pass through.
    pub(crate) fn with_read_cache(mut self, cache: Arc<ReadCache>) -> Self {
        self.store = Arc::new(CacheStore {
            inner: Arc::clone(&self.raw),
            cache: Arc::clone(&cache),
        });
        self.cache = Some(cache);
        self
    }

    /// The read cache, `None` without one.
    pub(crate) fn read_cache(&self) -> Option<&Arc<ReadCache>> {
        self.cache.as_ref()
    }

    /// The store without the read cache. Verification reads through it, so a
    /// corrupt cached block cannot make a healthy file look corrupt (spec 03 §10).
    pub(crate) fn raw_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.raw)
    }

    /// HEADs [`PROBE_KEY`] and, when that fails for any reason, writes it (D-N-36). The
    /// store answered when either call succeeds. A missing marker is a 404 from S3, but a
    /// 403 when the credentials lack `s3:ListBucket`, so any HEAD error leads to the PUT.
    /// Only the kinds of the two errors are logged: their text holds the URL.
    pub(crate) async fn probe(&self) -> Result<(), ProbeError> {
        let key = ObjectPath::from(PROBE_KEY);
        let raw = self.raw_store();
        let head = match raw.head(&key).await {
            Ok(_) => return Ok(()),
            Err(error) => error,
        };
        match raw.put(&key, PutPayload::from_static(b"")).await {
            Ok(_) => Ok(()),
            Err(put) => {
                tracing::debug!(
                    head = probe_kind(&head),
                    put = probe_kind(&put),
                    "object store probe failed"
                );
                Err(ProbeError::Unreachable)
            }
        }
    }

    /// The object store, for async Parquet reads at open.
    pub(crate) fn store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.store)
    }

    /// `tables/<table>/project=<project_id>/day=<YYYY-MM-DD>/<level>-<uuidv7>.parquet`.
    ///
    /// This is the only path formatter. `level` is `0` or `1`.
    pub(crate) fn table_file_path(
        table: FileTable,
        project: ProjectId,
        day: NaiveDate,
        level: u8,
    ) -> (String, FileId) {
        let id = FileId::new_v7();
        let path = format!(
            "tables/{}/project={project}/day={}/{level}-{id}.parquet",
            table.as_str(),
            day.format("%Y-%m-%d"),
        );
        (path, id)
    }

    /// `media/<project_id>/<sha256 hex>` (spec 03 §9).
    pub(crate) fn media_path(project: ProjectId, sha256_hex: &str) -> String {
        format!("media/{project}/{sha256_hex}")
    }

    /// Writes `bytes` so a later reader sees all of them or nothing.
    ///
    /// Local writes run on `spawn_blocking`. An existing final file is
    /// [`ObjectError::AlreadyExists`]. A remote error is [`ObjectError::Upload`]
    /// and is not retried here. A remote data file is written through the read
    /// cache after its upload.
    pub(crate) async fn put_durable(&self, path: &str, bytes: Bytes) -> Result<(), ObjectError> {
        self.refuse_after_sync_failure()?;
        validate_key(path)?;
        match &self.kind {
            Kind::Local { root } => {
                let root = root.clone();
                let key = path.to_owned();
                let hooks = clone_hooks(&self.hooks);
                let sync_failed = Arc::clone(&self.sync_failed);
                tokio::task::spawn_blocking(move || {
                    put_local(&root, &key, &bytes, &hooks, &sync_failed)
                })
                .await
                .map_err(|source| {
                    ObjectError::io("writing object", Path::new(path), io::Error::other(source))
                })?
            }
            Kind::Remote => {
                let through = self.write_through(path);
                let copy = through.as_ref().map(|_| bytes.clone());
                put_remote(&self.store, path, bytes, self.put_sizes).await?;
                if let (Some((cache, name)), Some(copy)) = (through, copy) {
                    // Best effort: an unwritten block is fetched from S3 later.
                    let _ =
                        tokio::task::spawn_blocking(move || cache.insert_bytes(&name, &copy)).await;
                }
                Ok(())
            }
        }
    }

    /// The read cache and the cache name of `key`, when a remote write of `key`
    /// is written through (a data file of a store with a read cache).
    fn write_through(&self, key: &str) -> Option<(Arc<ReadCache>, String)> {
        let cache = self.cache.as_ref()?;
        let name = cached_name(&object_key(key).ok()?)?;
        Some((Arc::clone(cache), name))
    }

    /// `false` in restore mode (D-N-34): every deletion is refused.
    pub(crate) fn deletes_enabled(&self) -> bool {
        self.delete_objects
    }

    /// Deletes one object. A missing object is success. Used by GC.
    pub(crate) async fn delete(&self, path: &str) -> Result<(), ObjectError> {
        if !self.delete_objects {
            return Err(ObjectError::DeletionOff);
        }
        self.refuse_after_sync_failure()?;
        validate_key(path)?;
        match &self.kind {
            Kind::Local { root } => {
                let root = root.clone();
                let key = path.to_owned();
                let sync_failed = Arc::clone(&self.sync_failed);
                tokio::task::spawn_blocking(move || delete_local(&root, &key, &sync_failed))
                    .await
                    .map_err(|source| {
                        ObjectError::io(
                            "deleting object",
                            Path::new(path),
                            io::Error::other(source),
                        )
                    })?
                    .map(|_| ())
            }
            Kind::Remote => {
                let location = object_key(path)?;
                match self.store.delete(&location).await {
                    Ok(()) => Ok(()),
                    Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(source) => Err(ObjectError::store("deleting object", path, source)),
                }
            }
        }
    }

    /// Writes a media blob under `path` (spec 03 §9).
    ///
    /// The name is the content's hash, so an existing object holds the same
    /// bytes: local [`ObjectError::AlreadyExists`] is success, and remote is a
    /// plain overwrite with no conditional put.
    pub(crate) async fn put_media(&self, path: &str, bytes: Bytes) -> Result<(), ObjectError> {
        match &self.kind {
            Kind::Local { .. } => match self.put_durable(path, bytes).await {
                Ok(()) => Ok(()),
                Err(ObjectError::AlreadyExists { .. }) => Ok(()),
                Err(error) => Err(error),
            },
            Kind::Remote => put_remote(&self.store, path, bytes, self.put_sizes).await,
        }
    }

    /// Reads `path`, whole or the byte `range` (end exclusive).
    pub(crate) async fn get_range(
        &self,
        path: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<Bytes, ObjectError> {
        validate_key(path)?;
        let location = object_key(path)?;
        let options = GetOptions {
            range: range.map(GetRange::Bounded),
            ..GetOptions::default()
        };
        let result = self
            .store
            .get_opts(&location, options)
            .await
            .map_err(|source| ObjectError::store("reading object", path, source))?;
        result
            .bytes()
            .await
            .map_err(|source| ObjectError::store("reading object", path, source))
    }

    /// Size of `path`, then [`Self::delete`]. A missing object is `0`.
    pub(crate) async fn delete_len(&self, path: &str) -> Result<u64, ObjectError> {
        if !self.delete_objects {
            return Err(ObjectError::DeletionOff);
        }
        self.refuse_after_sync_failure()?;
        validate_key(path)?;
        match &self.kind {
            Kind::Local { root } => {
                let root = root.clone();
                let key = path.to_owned();
                let sync_failed = Arc::clone(&self.sync_failed);
                tokio::task::spawn_blocking(move || delete_local(&root, &key, &sync_failed))
                    .await
                    .map_err(|source| {
                        ObjectError::io(
                            "deleting object",
                            Path::new(path),
                            io::Error::other(source),
                        )
                    })?
            }
            Kind::Remote => {
                let location = object_key(path)?;
                let bytes = match self.store.head(&location).await {
                    Ok(meta) => meta.size,
                    Err(object_store::Error::NotFound { .. }) => 0,
                    Err(source) => {
                        return Err(ObjectError::store("reading object metadata", path, source));
                    }
                };
                self.delete(path).await?;
                Ok(bytes)
            }
        }
    }

    /// Creates the temp file for the object `key` and returns it opened for
    /// reading and writing. The caller streams the object into it, then passes
    /// it to [`Self::publish_output`].
    pub(crate) async fn create_output(
        &self,
        key: &str,
    ) -> Result<(OutputTarget, fs::File), ObjectError> {
        match &self.kind {
            Kind::Local { root } => {
                self.refuse_after_sync_failure()?;
                validate_key(key)?;
                let root = root.clone();
                let key = key.to_owned();
                let key_for_error = key.clone();
                let sync_failed = Arc::clone(&self.sync_failed);
                let opened = tokio::task::spawn_blocking(move || {
                    let final_path = root.join(&key);
                    let Some(directory) = final_path.parent() else {
                        return Err(ObjectError::InvalidPath);
                    };
                    let Ok(relative_dir) = directory.strip_prefix(root.as_path()) else {
                        return Err(ObjectError::InvalidPath);
                    };
                    create_dir_chain_from(&root, relative_dir, &sync_failed)?;
                    let Some(file_name) = final_path.file_name() else {
                        return Err(ObjectError::InvalidPath);
                    };
                    let temp_path = directory.join(temp_name(file_name));
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&temp_path)
                        .map_err(|source| {
                            ObjectError::io("creating output temp file", &temp_path, source)
                        })?;
                    Ok((temp_path, file))
                })
                .await
                .map_err(|source| {
                    ObjectError::io(
                        "creating output temp file",
                        Path::new(&key_for_error),
                        io::Error::other(source),
                    )
                })??;
                Ok((
                    OutputTarget {
                        key: key_for_error,
                        temp: opened.0,
                    },
                    opened.1,
                ))
            }
            Kind::Remote => {
                // Scratch files: no fsync of the spill directory is needed.
                let temp = self.spill_dir.join(format!("{}.parquet", Uuid::now_v7()));
                let opened = tokio::task::spawn_blocking({
                    let temp = temp.clone();
                    let spill_dir = self.spill_dir.clone();
                    move || {
                        fs::create_dir_all(&spill_dir).map_err(|source| {
                            ObjectError::io("creating spill directory", &spill_dir, source)
                        })?;
                        OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create_new(true)
                            .open(&temp)
                            .map_err(|source| {
                                ObjectError::io("creating output temp file", &temp, source)
                            })
                    }
                })
                .await
                .map_err(|source| {
                    ObjectError::io("creating output temp file", &temp, io::Error::other(source))
                })??;
                Ok((
                    OutputTarget {
                        key: key.to_owned(),
                        temp,
                    },
                    opened,
                ))
            }
        }
    }

    /// Makes the finished temp file the object `key`, durably. Returns its size
    /// in bytes.
    pub(crate) async fn publish_output(
        &self,
        target: OutputTarget,
        file: fs::File,
    ) -> Result<u64, ObjectError> {
        match &self.kind {
            Kind::Local { root } => {
                if let Err(error) = self.refuse_after_sync_failure() {
                    // The output is dropped here, so its temp file goes with it (R5.5).
                    discard_temp(&target.temp);
                    return Err(error);
                }
                let root = root.clone();
                let key = target.key.clone();
                let temp = target.temp.clone();
                let hooks = clone_hooks(&self.hooks);
                let sync_failed = Arc::clone(&self.sync_failed);
                tokio::task::spawn_blocking(move || {
                    let size = match file.metadata() {
                        Ok(meta) => meta.len(),
                        Err(source) => {
                            discard_temp(&temp);
                            return Err(ObjectError::io("reading output size", &temp, source));
                        }
                    };
                    publish_local(&root, &key, &temp, file, &hooks, &sync_failed)?;
                    Ok(size)
                })
                .await
                .map_err(|source| {
                    ObjectError::io(
                        "publishing output",
                        Path::new(&target.key),
                        io::Error::other(source),
                    )
                })?
            }
            Kind::Remote => {
                let key = target.key;
                let temp = target.temp.clone();
                let temp_for_cleanup = target.temp.clone();
                let store = Arc::clone(&self.store);
                let through = self.write_through(&key);
                let sizes = self.put_sizes;
                let upload = async move {
                    let size = file
                        .metadata()
                        .map_err(|source| ObjectError::io("reading output size", &temp, source))?
                        .len();
                    let mut file = file;
                    file.seek(SeekFrom::Start(0))
                        .map_err(|source| ObjectError::io("rewinding output", &temp, source))?;
                    if usize::try_from(size).is_ok_and(|size| size <= sizes.single_max) {
                        let mut buffer = vec![0_u8; usize::try_from(size).unwrap_or(usize::MAX)];
                        file.read_exact(&mut buffer)
                            .map_err(|source| ObjectError::io("reading output", &temp, source))?;
                        let location = object_key(&key)?;
                        store
                            .put(&location, PutPayload::from(Bytes::from(buffer)))
                            .await
                            .map_or_else(
                                |source| Err(ObjectError::upload(&key, source)),
                                |_| Ok(()),
                            )?;
                    } else {
                        let reader = Arc::new(tokio::sync::Mutex::new(tokio::fs::File::from_std(
                            file.into_std(),
                        )));
                        put_multi(&store, &key, move || {
                            let reader = Arc::clone(&reader);
                            let temp = temp.clone();
                            async move {
                                let mut buffer = vec![0_u8; sizes.part];
                                let mut file = reader.lock().await;
                                let read =
                                    read_part(&mut *file, &mut buffer).await.map_err(|source| {
                                        ObjectError::io("reading output", &temp, source)
                                    })?;
                                if read == 0 {
                                    Ok(None)
                                } else {
                                    buffer.truncate(read);
                                    Ok(Some(PutPayload::from(Bytes::from(buffer))))
                                }
                            }
                        })
                        .await?;
                    }
                    Ok(size)
                }
                .await;
                if let (Some((cache, name)), Ok(size)) = (through, &upload) {
                    // Written through after the upload, from the spill file.
                    let size = *size;
                    let spill = temp_for_cleanup.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut file) = fs::File::open(&spill) {
                            cache.insert_file(&name, &mut file, size);
                        }
                    })
                    .await;
                }
                // The spill file is scratch: remove it either way.
                let removed = remove_spill_file(&temp_for_cleanup).await;
                let size = upload?;
                removed?;
                Ok(size)
            }
        }
    }

    /// Removes a temp file that will not be published. A missing file is success.
    pub(crate) async fn discard_output(&self, target: OutputTarget) -> Result<(), ObjectError> {
        let temp = target.temp.clone();
        tokio::task::spawn_blocking(move || remove_temp_path(&temp))
            .await
            .map_err(|source| {
                ObjectError::io(
                    "removing output temp file",
                    &target.temp,
                    io::Error::other(source),
                )
            })?
    }

    /// Deletes leftover temp files: under `tables/` and `media/` in local
    /// mode, every file of the spill directory in remote mode.
    ///
    /// In local mode the walk fsyncs every directory it visits, children
    /// first, then the root, so the removals and the `tables` and `media`
    /// entries are durable before the first write of this life.
    ///
    /// This walks the filesystem on the calling thread; open calls it from
    /// `spawn_blocking`.
    pub(crate) fn remove_temp_files(&self) -> Result<u64, ObjectError> {
        match &self.kind {
            Kind::Local { root } => {
                let mut removed = 0;
                remove_temps(&root.join("tables"), &mut removed, &self.sync_failed)?;
                remove_temps(&root.join("media"), &mut removed, &self.sync_failed)?;
                // The `tables` and `media` entries live in the root itself.
                fs::sync_dir(root).map_err(|source| {
                    sync_failure(&self.sync_failed, "syncing object directory", root, source)
                })?;
                Ok(removed)
            }
            Kind::Remote => {
                let entries = match fs::read_dir(&self.spill_dir) {
                    Ok(entries) => entries,
                    Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(0),
                    Err(source) => {
                        return Err(ObjectError::io(
                            "reading spill directory",
                            &self.spill_dir,
                            source,
                        ));
                    }
                };
                let mut removed = 0;
                for entry in entries {
                    let entry = entry.map_err(|source| {
                        ObjectError::io("reading spill directory", &self.spill_dir, source)
                    })?;
                    if entry
                        .file_type()
                        .map_err(|source| {
                            ObjectError::io("reading spill directory", &entry.path(), source)
                        })?
                        .is_file()
                    {
                        fs::remove_file(entry.path()).map_err(|source| {
                            ObjectError::io("removing output temp file", &entry.path(), source)
                        })?;
                        removed += 1;
                    }
                }
                Ok(removed)
            }
        }
    }
}

/// Where an output is being written before it is published as `key`.
#[derive(Debug)]
pub(crate) struct OutputTarget {
    key: String,
    temp: PathBuf,
}

impl OutputTarget {
    /// The temp file's path, for error messages.
    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }
}

#[allow(clippy::clone_on_copy)]
fn clone_hooks(hooks: &Hooks) -> Hooks {
    hooks.clone()
}

fn prepare_local_root(
    root: &Path,
    data_dir: &Path,
) -> Result<(PathBuf, LocalFileSystem), ObjectError> {
    create_dir_chain(root, data_dir)?;
    let root = fs::canonicalize(root)
        .map_err(|source| ObjectError::io("opening object store", root, source))?;
    let store = LocalFileSystem::new_with_prefix(&root).map_err(ObjectError::client)?;
    Ok((root, store))
}

fn build_s3(
    bucket: &str,
    prefix: &str,
    s3: &S3Config,
) -> Result<Arc<dyn ObjectStore>, ObjectError> {
    let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
    if let Some(region) = &s3.region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = &s3.endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    // `force_path_style` is the config flag. The builder's flag is the opposite.
    builder = builder
        .with_virtual_hosted_style_request(!s3.force_path_style)
        .with_allow_http(s3.allow_http);
    let store = builder.build().map_err(ObjectError::client)?;
    if prefix.is_empty() {
        Ok(Arc::new(store))
    } else {
        Ok(Arc::new(PrefixStore::new(store, prefix)))
    }
}

fn put_local(
    root: &Path,
    key: &str,
    bytes: &Bytes,
    hooks: &Hooks,
    sync_failed: &AtomicBool,
) -> Result<(), ObjectError> {
    let final_path = root.join(key);
    if try_exists(&final_path)? {
        return Err(ObjectError::AlreadyExists {
            path: key.to_owned(),
        });
    }
    let Some(directory) = final_path.parent() else {
        return Err(ObjectError::InvalidPath);
    };
    let Ok(relative_dir) = directory.strip_prefix(root) else {
        return Err(ObjectError::InvalidPath);
    };
    create_dir_chain_from(root, relative_dir, sync_failed)?;

    let Some(file_name) = final_path.file_name() else {
        return Err(ObjectError::InvalidPath);
    };
    let temp_path = directory.join(temp_name(file_name));

    hooks.pause(HookPoint::ObjectWrite);
    if let Some(source) = hooks.fail(HookPoint::ObjectWrite) {
        return Err(ObjectError::io("writing object", &temp_path, source));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|source| ObjectError::io("writing object", &temp_path, source))?;
    if let Err(source) = file.write_all(bytes) {
        discard_temp(&temp_path);
        return Err(ObjectError::io("writing object", &temp_path, source));
    }

    publish_local(root, key, &temp_path, file, hooks, sync_failed)
}

/// The publication tail shared by `put_local` and `Objects::publish_output`:
/// `sync_all`, refuse an existing final name (removing the temp file), rename,
/// then fsync the directory. An `ObjectDirSync` fault renames the file back.
///
/// A failure before the rename removes the temp file (R5.5). A real `sync_dir`
/// failure after the rename does not: the bytes are already under the final name.
fn publish_local(
    root: &Path,
    key: &str,
    temp_path: &Path,
    file: fs::File,
    hooks: &Hooks,
    sync_failed: &AtomicBool,
) -> Result<(), ObjectError> {
    let final_path = root.join(key);
    let Some(directory) = final_path.parent() else {
        return Err(ObjectError::InvalidPath);
    };

    hooks.pause(HookPoint::ObjectSync);
    if let Some(source) = hooks.fail(HookPoint::ObjectSync) {
        discard_temp(temp_path);
        return Err(sync_failure(
            sync_failed,
            "syncing object",
            temp_path,
            source,
        ));
    }
    if let Err(source) = file.sync_all() {
        discard_temp(temp_path);
        return Err(sync_failure(
            sync_failed,
            "syncing object",
            temp_path,
            source,
        ));
    }
    drop(file);

    hooks.pause(HookPoint::ObjectRename);
    if let Some(source) = hooks.fail(HookPoint::ObjectRename) {
        discard_temp(temp_path);
        return Err(ObjectError::io("renaming object", &final_path, source));
    }
    let exists = match try_exists(&final_path) {
        Ok(exists) => exists,
        Err(error) => {
            discard_temp(temp_path);
            return Err(error);
        }
    };
    if exists {
        // Names are unique, so this is a retry that actually landed, or a
        // caller bug. Do not leave the temp file behind.
        discard_temp(temp_path);
        return Err(ObjectError::AlreadyExists {
            path: key.to_owned(),
        });
    }
    if let Err(source) = fs::rename(temp_path, &final_path) {
        discard_temp(temp_path);
        return Err(ObjectError::io("renaming object", &final_path, source));
    }

    hooks.pause(HookPoint::ObjectDirSync);
    if let Some(source) = hooks.fail(HookPoint::ObjectDirSync) {
        let error = sync_failure(sync_failed, "syncing object directory", directory, source);
        // The rename already published the final name. Move it back so a failed
        // publish does not leave that name, and startup cleanup can drop the
        // temp file. A real `sync_dir` error below is not undone: the bytes
        // are already under the final name.
        fs::rename(&final_path, temp_path)
            .map_err(|undo| ObjectError::io("restoring object temp file", temp_path, undo))?;
        return Err(error);
    }
    fs::sync_dir(directory).map_err(|source| {
        sync_failure(sync_failed, "syncing object directory", directory, source)
    })?;
    Ok(())
}

/// Records a failed local fsync, so every later local write and delete is
/// refused until a restart (D-R-6), and returns the error for the caller.
/// Logs once, at the first failure. Logs no key: the object key is debug only.
fn sync_failure(
    sync_failed: &AtomicBool,
    action: &'static str,
    path: &Path,
    source: io::Error,
) -> ObjectError {
    if !sync_failed.swap(true, Ordering::SeqCst) {
        tracing::error!(action, "object store stopped after a failed fsync");
    }
    ObjectError::io(action, path, source)
}

async fn put_remote(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    bytes: Bytes,
    sizes: PutSizes,
) -> Result<(), ObjectError> {
    let location = object_key(path)?;
    if bytes.len() <= sizes.single_max {
        return store
            .put(&location, PutPayload::from(bytes))
            .await
            .map_or_else(|source| Err(ObjectError::upload(path, source)), |_| Ok(()));
    }
    let mut remaining = bytes;
    put_multi(store, path, move || {
        let part = if remaining.is_empty() {
            None
        } else {
            Some(PutPayload::from(
                remaining.split_to(sizes.part.min(remaining.len())),
            ))
        };
        std::future::ready(Ok(part))
    })
    .await
}

/// One multipart upload, taking one part at a time from `next`. Any error after
/// the upload started (a part, the part source or `complete`) aborts it, best
/// effort, and returns the original error (spec 03 §2).
async fn put_multi<F, Fut>(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    mut next: F,
) -> Result<(), ObjectError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<PutPayload>, ObjectError>>,
{
    let location = object_key(path)?;
    let mut upload = store
        .put_multipart(&location)
        .await
        .map_err(|source| ObjectError::upload(path, source))?;
    let outcome = send_parts(upload.as_mut(), path, &mut next).await;
    if outcome.is_err() {
        // A failed `complete` may still have created the object. Aborting cannot
        // remove a completed object (S3 answers NoSuchUpload); it only frees the
        // parts when the upload really failed. The name stays staged either way,
        // and GC removes it (spec 03 §2).
        if upload.abort().await.is_err() {
            tracing::warn!(key = %path, "aborting a failed multipart upload failed; the parts stay until the lifecycle rule removes them");
        }
    }
    outcome
}

/// Sends every part from `next` to `upload`, then completes it.
async fn send_parts<F, Fut>(
    upload: &mut dyn MultipartUpload,
    path: &str,
    next: &mut F,
) -> Result<(), ObjectError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<PutPayload>, ObjectError>>,
{
    while let Some(part) = next().await? {
        upload
            .put_part(part)
            .await
            .map_err(|source| ObjectError::upload(path, source))?;
    }
    upload
        .complete()
        .await
        .map(|_| ())
        .map_err(|source| ObjectError::upload(path, source))
}

/// Unlinks the local object `key` under `root` and returns its size before the
/// unlink. A missing object is success, with size 0.
///
/// The parent directory is fsynced after the unlink, so the removal is durable
/// before the catalog row goes. It is synced even when this call found nothing
/// to remove: a previous attempt may have unlinked the file without the sync.
/// A missing parent has nothing to make durable.
fn delete_local(root: &Path, key: &str, sync_failed: &AtomicBool) -> Result<u64, ObjectError> {
    let path = root.join(key);
    let Some(parent) = path.parent() else {
        return Err(ObjectError::InvalidPath);
    };
    let size = match fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(source) if source.kind() == io::ErrorKind::NotFound => 0,
        Err(source) => return Err(ObjectError::io("reading object metadata", &path, source)),
    };
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(ObjectError::io("deleting object", &path, source)),
    }
    match fs::sync_dir(parent) {
        Ok(()) => Ok(size),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(size),
        Err(source) => Err(sync_failure(
            sync_failed,
            "syncing object directory",
            parent,
            source,
        )),
    }
}

/// Removes the temp file of a write that failed, so its space comes back now
/// and not at the next start (R5.5). The caller returns the write's error;
/// a failed removal is only logged, with no path. Not a fsync: a removal that
/// a crash undoes is cleaned up at the next start.
fn discard_temp(temp: &Path) {
    if remove_temp_path(temp).is_err() {
        tracing::warn!("removing a failed object temp file failed");
    }
}

/// Removes one temp file; a missing file is success.
fn remove_temp_path(temp: &Path) -> Result<(), ObjectError> {
    match fs::remove_file(temp) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ObjectError::io("removing output temp file", temp, source)),
    }
}

/// Removes a spill file from the blocking pool. A missing file is success.
async fn remove_spill_file(temp: &Path) -> Result<(), ObjectError> {
    let moved = temp.to_path_buf();
    tokio::task::spawn_blocking(move || remove_temp_path(&moved))
        .await
        .map_err(|source| {
            ObjectError::io("removing output temp file", temp, io::Error::other(source))
        })?
}

fn temp_name(file_name: &OsStr) -> OsString {
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".tmp-{}", Uuid::now_v7()));
    name
}

pub(crate) fn is_temp_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.starts_with('.') && name.contains(".tmp-")
}

/// Removes the temp files under `dir` and fsyncs every directory it walks,
/// children first. A directory a crashed process created without a sync is then
/// durable before the first write of this life. A missing `dir` is success.
fn remove_temps(
    dir: &Path,
    removed: &mut u64,
    sync_failed: &AtomicBool,
) -> Result<(), ObjectError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(ObjectError::io("reading object directory", dir, source)),
    };
    for entry in entries {
        let entry =
            entry.map_err(|source| ObjectError::io("reading object directory", dir, source))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| ObjectError::io("reading object directory", &path, source))?;
        if kind.is_dir() {
            remove_temps(&path, removed, sync_failed)?;
        } else if kind.is_file() && is_temp_name(&entry.file_name()) {
            fs::remove_file(&path)
                .map_err(|source| ObjectError::io("removing object temp file", &path, source))?;
            *removed += 1;
        }
    }
    fs::sync_dir(dir)
        .map_err(|source| sync_failure(sync_failed, "syncing object directory", dir, source))
}

/// Creates `path` and any missing parent, then fsyncs the parent of every
/// directory of the chain, whether this call created it or not. A process
/// killed after `create_dir` and before its sync leaves an entry that every
/// later call takes for existing, so the sync must not depend on who created
/// the entry.
///
/// The walk goes down. It starts at `data_dir` when `path` is inside it, and
/// below the nearest existing ancestor otherwise, so it never syncs from `/`
/// down into directories the process does not own. Runs at open only.
fn create_dir_chain(path: &Path, data_dir: &Path) -> Result<(), ObjectError> {
    let inside = path
        .strip_prefix(data_dir)
        .is_ok_and(|relative| relative.components().next().is_some());
    let base = if inside {
        data_dir.to_path_buf()
    } else {
        nearest_existing_ancestor(path)?
    };
    let Ok(relative) = path.strip_prefix(&base) else {
        return Err(ObjectError::InvalidPath);
    };
    if !inside {
        // `base` may have been created by a process killed before its sync,
        // and the walk below never syncs `base`'s own entry in its parent.
        sync_parent(&base)?;
    }
    let mut current = base;
    for segment in relative.components() {
        current.push(segment);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists && current.is_dir() => {}
            Err(source) => {
                return Err(ObjectError::io(
                    "creating object directory",
                    &current,
                    source,
                ));
            }
        }
        sync_parent(&current)?;
    }
    Ok(())
}

/// Fsyncs the directory that holds the entry `path`.
fn sync_parent(path: &Path) -> Result<(), ObjectError> {
    match fs::parent_dir(path) {
        Some(parent) => fs::sync_dir(parent)
            .map_err(|source| ObjectError::io("syncing object directory", parent, source)),
        None => Ok(()),
    }
}

/// The nearest ancestor of `path` (or `path` itself) that exists. It must be a directory.
/// A one-component relative path climbs to `.`, not to the empty path.
fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, ObjectError> {
    let mut cursor = path;
    loop {
        if try_exists(cursor)? {
            if !cursor.is_dir() {
                return Err(ObjectError::io(
                    "creating object directory",
                    cursor,
                    io::Error::other("not a directory"),
                ));
            }
            return Ok(cursor.to_path_buf());
        }
        cursor = fs::parent_dir(cursor).ok_or_else(|| {
            ObjectError::io(
                "creating object directory",
                path,
                io::Error::other("not a directory"),
            )
        })?;
    }
}

/// Creates the directories of `relative` under `root`, syncing each parent
/// after its `create_dir`. A failed sync is a failed local fsync: it sets
/// `sync_failed` (D-R-6), because a retry would find the directory and skip
/// its sync.
fn create_dir_chain_from(
    root: &Path,
    relative: &Path,
    sync_failed: &AtomicBool,
) -> Result<(), ObjectError> {
    let mut current = root.to_path_buf();
    for segment in relative.components() {
        current.push(segment);
        if try_exists(&current)? {
            if current.is_dir() {
                continue;
            }
            return Err(ObjectError::io(
                "creating object directory",
                &current,
                io::Error::other("not a directory"),
            ));
        }
        match fs::create_dir(&current) {
            Ok(()) => {
                if let Some(parent) = current.parent() {
                    fs::sync_dir(parent).map_err(|source| {
                        sync_failure(sync_failed, "syncing object directory", parent, source)
                    })?;
                }
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists && current.is_dir() => {}
            Err(source) => {
                return Err(ObjectError::io(
                    "creating object directory",
                    &current,
                    source,
                ));
            }
        }
    }
    Ok(())
}

fn try_exists(path: &Path) -> Result<bool, ObjectError> {
    fs::try_exists(path).map_err(|source| ObjectError::io("checking object", path, source))
}

fn validate_key(path: &str) -> Result<(), ObjectError> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
        return Err(ObjectError::InvalidPath);
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ObjectError::InvalidPath);
    }
    object_key(path).map(|_| ())
}

fn object_key(path: &str) -> Result<ObjectPath, ObjectError> {
    ObjectPath::parse(path).map_err(|_| ObjectError::InvalidPath)
}

/// Reads until `buffer` is full or the reader ends, and returns the bytes read.
///
/// A single `read` may return less than asked, and S3 refuses a multipart part
/// other than the last under 5 MiB, so each part is filled before it is sent.
async fn read_part<R>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin + ?Sized,
{
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_multipart_part_is_filled_across_short_reads() {
        use tokio::io::AsyncReadExt as _;
        // Three sources chained: each `read` stops at a source boundary.
        let first = [1_u8; 3];
        let second = [2_u8; 4];
        let third = [3_u8; 5];
        let mut reader = (&first[..]).chain(&second[..]).chain(&third[..]);
        let mut buffer = [0_u8; 10];
        assert_eq!(
            super::read_part(&mut reader, &mut buffer).await.unwrap(),
            10
        );
        assert_eq!(buffer, [1, 1, 1, 2, 2, 2, 2, 3, 3, 3]);
        let mut rest = [0_u8; 10];
        assert_eq!(super::read_part(&mut reader, &mut rest).await.unwrap(), 2);
        assert_eq!(rest[..2], [3, 3]);
        assert_eq!(super::read_part(&mut reader, &mut rest).await.unwrap(), 0);
    }

    use bytesize::ByteSize;
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use cetra_core::ids::ProjectId;
    use chrono::NaiveDate;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;

    use super::{PutSizes, is_temp_name, put_multi};
    use crate::catalog::FileTable;
    use crate::config::{
        FlushConfig, MemtableConfig, ObjectStoreUrl, QueryConfig, S3Config, StorageConfig,
        WalConfig,
    };
    use crate::error::ObjectError;
    use crate::fs::lossy::{CrashMode, Lossy};
    use crate::hooks::{HookPoint, Hooks};
    use crate::objects::Objects;
    use crate::test_support::{Call, ChaosStore, Fault, Status, UploadState};

    /// Multipart from 1 KiB, in 512-byte parts: small stand-ins for the S3 sizes, so the tests
    /// never allocate 64 MiB.
    const SIZES: PutSizes = PutSizes {
        single_max: 1024,
        part: 512,
    };

    /// A remote `Objects` over `chaos`, uploading with [`SIZES`].
    fn remote_objects(chaos: &ChaosStore, dir: &std::path::Path) -> Objects {
        Objects::remote_for_test(Arc::new(chaos.clone()), dir, Hooks::default(), SIZES).unwrap()
    }

    /// Six parts under [`SIZES`]: five of 512 bytes and one of 440.
    fn multipart_bytes() -> Bytes {
        Bytes::from(vec![5_u8; 3000])
    }

    /// The states of the uploads in `chaos`'s ledger, in order.
    fn states(chaos: &ChaosStore) -> Vec<UploadState> {
        chaos.uploads().iter().map(|record| record.state).collect()
    }

    fn local_config(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: Some(ObjectStoreUrl::Local(dir.join("objects"))),
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

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    async fn read_registered(objects: &Objects, key: &str) -> Bytes {
        let location = object_store::path::Path::parse(key).unwrap();
        objects
            .store()
            .get(&location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    }

    fn temps(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    walk(&path, found);
                } else if kind.is_file() && is_temp_name(&entry.file_name()) {
                    found.push(path);
                }
            }
        }
        walk(&root.join("tables"), &mut found);
        found
    }

    #[tokio::test]
    async fn local_put_is_durable_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::default();
        let objects = Objects::new(&local_config(dir.path()), hooks.clone())
            .await
            .unwrap();
        let (key, id) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let bytes = Bytes::from_static(b"parquet-bytes");
        objects.put_durable(&key, bytes.clone()).await.unwrap();

        let root = dir.path().join("objects");
        let final_path = root.join(&key);
        assert_eq!(std::fs::read(&final_path).unwrap(), bytes.as_ref());
        assert!(temps(&root).is_empty());
        assert_eq!(
            hooks.points(),
            vec![
                HookPoint::ObjectWrite,
                HookPoint::ObjectSync,
                HookPoint::ObjectRename,
                HookPoint::ObjectDirSync,
            ]
        );
        assert_eq!(read_registered(&objects, &key).await, bytes);
        assert!(key.contains(&id.to_string()));

        let again = objects.put_durable(&key, bytes).await.unwrap_err();
        assert!(matches!(again, ObjectError::AlreadyExists { .. }));
        assert!(temps(&root).is_empty(), "AlreadyExists left a temp file");

        objects.delete(&key).await.unwrap();
        assert!(!final_path.exists());
        objects.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn local_faults_leave_no_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("objects");
        for point in [
            HookPoint::ObjectWrite,
            HookPoint::ObjectSync,
            HookPoint::ObjectRename,
            HookPoint::ObjectDirSync,
        ] {
            let hooks = Hooks::default();
            let objects = Objects::new(&local_config(dir.path()), hooks.clone())
                .await
                .unwrap();
            hooks.arm_fail(point);
            let (key, _) = Objects::table_file_path(FileTable::Scores, project(), day(), 1);
            let error = objects
                .put_durable(&key, Bytes::from_static(b"nope"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ObjectError::Io { .. }),
                "{point:?} returned {error}"
            );
            assert!(!root.join(&key).exists(), "{point:?} left the final file");
            assert_eq!(
                objects.sync_failed(),
                matches!(point, HookPoint::ObjectSync | HookPoint::ObjectDirSync),
                "{point:?}"
            );
            // A failed sync or rename removes its temp file (R5.5). A failed
            // directory sync renames the file back on purpose, for the crash
            // tests, so startup cleanup finds it.
            let removed = objects.remove_temp_files().unwrap();
            let expected = u64::from(point == HookPoint::ObjectDirSync);
            assert_eq!(removed, expected, "{point:?}");
            assert!(temps(&root).is_empty(), "{point:?}");
            assert!(!root.join(&key).exists());
        }
    }

    /// R5.5: a write whose sync or rename fails removes its temp file at once,
    /// not at the next start.
    #[tokio::test]
    async fn a_failed_local_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("objects");
        for point in [HookPoint::ObjectSync, HookPoint::ObjectRename] {
            let hooks = Hooks::default();
            let objects = Objects::new(&local_config(dir.path()), hooks.clone())
                .await
                .unwrap();
            hooks.arm_fail(point);
            let (key, _) = Objects::table_file_path(FileTable::Scores, project(), day(), 1);
            let error = objects
                .put_durable(&key, Bytes::from_static(b"nope"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ObjectError::Io { .. }),
                "{point:?} returned {error}"
            );
            assert!(temps(&root).is_empty(), "{point:?} left a temp file");
            assert!(!root.join(&key).exists(), "{point:?} left the final file");
        }
    }

    #[tokio::test]
    async fn remote_put_round_trips_and_reports_faults() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as Arc<dyn object_store::ObjectStore>);
        let objects =
            Objects::remote_for_test(Arc::new(fault.clone()), dir.path(), Hooks::default(), SIZES)
                .unwrap();
        let small = Bytes::from_static(b"small");
        objects
            .put_durable("tables/spans/a.parquet", small.clone())
            .await
            .unwrap();
        assert_eq!(
            read_registered(&objects, "tables/spans/a.parquet").await,
            small
        );

        let mut large = vec![0_u8; SIZES.single_max + 1];
        large[0] = 1;
        large[SIZES.part] = 2;
        large[SIZES.single_max] = 3;
        let large = Bytes::from(large);
        objects
            .put_durable("tables/spans/b.parquet", large.clone())
            .await
            .unwrap();
        assert_eq!(
            read_registered(&objects, "tables/spans/b.parquet").await,
            large
        );
        assert_eq!(states(&fault), vec![UploadState::Completed]);
        assert_eq!(fault.count(Call::Abort), 0);
        // Every part but the last is one full part; the last holds the rest.
        let parts = fault.uploads()[0].parts.clone();
        assert_eq!(parts.iter().sum::<usize>(), SIZES.single_max + 1);
        let (last, full) = parts.split_last().expect("the upload sent parts");
        assert!(full.iter().all(|&len| len == SIZES.part), "{parts:?}");
        assert!(*last >= 1 && *last <= SIZES.part, "{parts:?}");

        fault.push(Call::Put, Fault::Before(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/c.parquet", Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }));
        let missing = object_store::path::Path::parse("tables/spans/c.parquet").unwrap();
        assert!(matches!(
            inner.get(&missing).await.unwrap_err(),
            object_store::Error::NotFound { .. }
        ));

        fault.push(Call::Put, Fault::After(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/d.parquet", Bytes::from_static(b"landed"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }));
        assert_eq!(
            read_registered(&objects, "tables/spans/d.parquet").await,
            Bytes::from_static(b"landed")
        );

        fault.push(Call::Put, Fault::Delay(Duration::from_millis(20)));
        objects
            .put_durable("tables/spans/e.parquet", Bytes::from_static(b"slow"))
            .await
            .unwrap();
        assert_eq!(
            read_registered(&objects, "tables/spans/e.parquet").await,
            Bytes::from_static(b"slow")
        );

        fault.push(Call::Complete, Fault::After(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/f.parquet", large)
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }));
        let landed = read_registered(&objects, "tables/spans/f.parquet").await;
        assert_eq!(landed.len(), SIZES.single_max + 1);
        assert_eq!(objects.remove_temp_files().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_local_output_publishes_durably() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::default();
        let objects = Objects::new(&local_config(dir.path()), hooks.clone())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let (target, mut file) = objects.create_output(&key).await.unwrap();
        file.write_all(b"0123456789").unwrap();
        let size = objects.publish_output(target, file).await.unwrap();
        assert_eq!(size, 10);
        let root = dir.path().join("objects");
        assert_eq!(std::fs::read(root.join(&key)).unwrap(), b"0123456789");
        assert!(temps(&root).is_empty());
        // The writing happened through `FileWriter`, so no `ObjectWrite`.
        assert_eq!(
            hooks.points(),
            vec![
                HookPoint::ObjectSync,
                HookPoint::ObjectRename,
                HookPoint::ObjectDirSync,
            ]
        );
    }

    #[tokio::test]
    async fn a_local_output_over_an_existing_final_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let objects = Objects::new(&local_config(dir.path()), Hooks::default())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Scores, project(), day(), 1);
        objects
            .put_durable(&key, Bytes::from_static(b"existing"))
            .await
            .unwrap();
        let (target, mut file) = objects.create_output(&key).await.unwrap();
        file.write_all(b"second").unwrap();
        let error = objects.publish_output(target, file).await.unwrap_err();
        assert!(
            matches!(error, ObjectError::AlreadyExists { .. }),
            "{error}"
        );
        let root = dir.path().join("objects");
        assert!(temps(&root).is_empty(), "refused publish left a temp file");
        assert_eq!(std::fs::read(root.join(&key)).unwrap(), b"existing");
    }

    #[tokio::test]
    async fn a_discarded_output_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let objects = Objects::new(&local_config(dir.path()), Hooks::default())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);

        let (target, mut file) = objects.create_output(&key).await.unwrap();
        file.write_all(b"never").unwrap();
        drop(file);
        assert!(target.temp.is_file());
        objects.discard_output(target).await.unwrap();

        // A leftover temp file from `create_output` is startup cleanup's job.
        let (target, file) = objects.create_output(&key).await.unwrap();
        drop(file);
        assert!(target.temp.is_file());
        let root = dir.path().join("objects");
        assert_eq!(objects.remove_temp_files().unwrap(), 1);
        assert!(temps(&root).is_empty());
    }

    #[tokio::test]
    async fn remote_outputs_upload_and_leave_no_spill_file() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(InMemory::new());
        let objects = Objects::remote_for_test(
            Arc::clone(&inner) as Arc<dyn object_store::ObjectStore>,
            dir.path(),
            Hooks::default(),
            SIZES,
        )
        .unwrap();
        let spill = dir.path().join("tmp").join("compaction");

        let small = vec![7_u8; 1024];
        let (target, mut file) = objects
            .create_output("tables/spans/small.parquet")
            .await
            .unwrap();
        assert!(target.temp.starts_with(&spill));
        let temp = target.temp.clone();
        file.write_all(&small).unwrap();
        let size = objects.publish_output(target, file).await.unwrap();
        assert_eq!(size, 1024);
        assert_eq!(
            read_registered(&objects, "tables/spans/small.parquet").await,
            Bytes::from(small)
        );
        assert!(!temp.exists());
        assert!(std::fs::read_dir(&spill).unwrap().next().is_none());

        let mut large = vec![0_u8; SIZES.single_max + 1];
        large[0] = 1;
        large[SIZES.part] = 2;
        large[SIZES.single_max] = 3;
        let (target, mut file) = objects
            .create_output("tables/spans/large.parquet")
            .await
            .unwrap();
        let temp = target.temp.clone();
        file.write_all(&large).unwrap();
        let size = objects.publish_output(target, file).await.unwrap();
        assert_eq!(size, u64::try_from(large.len()).unwrap());
        assert_eq!(
            read_registered(&objects, "tables/spans/large.parquet").await,
            Bytes::from(large)
        );
        assert!(!temp.exists());

        let (target, file) = objects
            .create_output("tables/spans/leftover.parquet")
            .await
            .unwrap();
        drop(file);
        assert!(target.temp.is_file());
        assert_eq!(objects.remove_temp_files().unwrap(), 1);
        assert!(!target.temp.exists());
    }

    #[tokio::test]
    async fn a_failed_remote_output_upload_removes_the_spill_file() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(InMemory::new());
        let fault = ChaosStore::new(Arc::clone(&inner) as Arc<dyn object_store::ObjectStore>);
        let objects =
            Objects::remote_for_test(Arc::new(fault.clone()), dir.path(), Hooks::default(), SIZES)
                .unwrap();
        fault.push(Call::MultipartStart, Fault::Before(Status::SlowDown));
        let (target, mut file) = objects
            .create_output("tables/spans/fail.parquet")
            .await
            .unwrap();
        let temp = target.temp.clone();
        let payload = vec![9_u8; SIZES.single_max + 1];
        file.write_all(&payload).unwrap();
        let error = objects.publish_output(target, file).await.unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }), "{error}");
        assert!(!temp.exists(), "failed upload left the spill file");
        let missing = object_store::path::Path::parse("tables/spans/fail.parquet").unwrap();
        assert!(matches!(
            inner.get(&missing).await.unwrap_err(),
            object_store::Error::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn a_failed_part_aborts_the_upload() {
        let dir = tempfile::tempdir().unwrap();
        let chaos = ChaosStore::new(Arc::new(InMemory::new()));
        let objects = remote_objects(&chaos, dir.path());
        chaos.push(Call::PutPart, Fault::Before(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/part.parquet", multipart_bytes())
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }), "{error}");
        assert_eq!(states(&chaos), vec![UploadState::Aborted]);
        assert_eq!(chaos.count(Call::Abort), 1);
        assert!(chaos.leaked_uploads().is_empty());
    }

    #[tokio::test]
    async fn a_failed_complete_aborts_the_upload() {
        let dir = tempfile::tempdir().unwrap();
        let chaos = ChaosStore::new(Arc::new(InMemory::new()));
        let objects = remote_objects(&chaos, dir.path());
        chaos.push(Call::Complete, Fault::Before(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/complete.parquet", multipart_bytes())
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }), "{error}");
        assert_eq!(states(&chaos), vec![UploadState::Aborted]);
        assert_eq!(chaos.count(Call::Abort), 1);
        let missing = object_store::path::Path::parse("tables/spans/complete.parquet").unwrap();
        assert!(matches!(
            chaos.inner().head(&missing).await.unwrap_err(),
            object_store::Error::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn an_ambiguous_complete_aborts_and_keeps_the_object() {
        let dir = tempfile::tempdir().unwrap();
        let chaos = ChaosStore::new(Arc::new(InMemory::new()));
        let objects = remote_objects(&chaos, dir.path());
        chaos.push(Call::Complete, Fault::After(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/ambiguous.parquet", multipart_bytes())
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }), "{error}");
        assert_eq!(states(&chaos), vec![UploadState::Aborted]);
        assert_eq!(chaos.count(Call::Abort), 1);
        // Abort cannot remove an object that `complete` created (S3 answers NoSuchUpload).
        let landed = object_store::path::Path::parse("tables/spans/ambiguous.parquet").unwrap();
        assert_eq!(chaos.inner().head(&landed).await.unwrap().size, 3000);
    }

    #[tokio::test]
    async fn a_failed_part_read_aborts_the_upload() {
        let chaos = ChaosStore::new(Arc::new(InMemory::new()));
        let store: Arc<dyn object_store::ObjectStore> = Arc::new(chaos.clone());
        // One part, then the part source fails.
        let mut reads = 0;
        let next = || {
            reads += 1;
            std::future::ready(if reads == 1 {
                Ok(Some(object_store::PutPayload::from(Bytes::from(vec![
                    1_u8;
                    512
                ]))))
            } else {
                Err(ObjectError::InvalidPath)
            })
        };
        let error = put_multi(&store, "tables/spans/read.parquet", next)
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::InvalidPath), "{error}");
        assert_eq!(states(&chaos), vec![UploadState::Aborted]);
        assert_eq!(chaos.count(Call::Abort), 1);
    }

    #[tokio::test]
    async fn a_failed_abort_is_reported_as_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let chaos = ChaosStore::new(Arc::new(InMemory::new()));
        let objects = remote_objects(&chaos, dir.path());
        chaos.push(Call::PutPart, Fault::Before(Status::SlowDown));
        chaos.push(Call::Abort, Fault::Before(Status::SlowDown));
        let error = objects
            .put_durable("tables/spans/stuck.parquet", multipart_bytes())
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Upload { .. }), "{error}");
        let abandoned: Vec<String> = chaos
            .abandoned_uploads()
            .into_iter()
            .map(|record| record.path)
            .collect();
        assert_eq!(abandoned, vec!["tables/spans/stuck.parquet".to_owned()]);
        assert_eq!(chaos.count(Call::Abort), 1);
        assert!(chaos.leaked_uploads().is_empty());
    }

    #[test]
    fn table_file_path_matches_the_layout_and_is_unique() {
        let (first, first_id) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let (second, second_id) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        assert_ne!(first_id, second_id);
        assert_ne!(first, second);
        assert_eq!(
            first,
            format!(
                "tables/spans/project={}/day=2026-11-11/0-{first_id}.parquet",
                project()
            )
        );
        let (scores, score_id) = Objects::table_file_path(FileTable::Scores, project(), day(), 1);
        assert_eq!(
            scores,
            format!(
                "tables/scores/project={}/day=2026-11-11/1-{score_id}.parquet",
                project()
            )
        );
        // The literal layout: the id is the only part that varies.
        assert_eq!(
            first.replace(&first_id.to_string(), "<id>"),
            "tables/spans/project=018f6b4e-7c1a-7c1a-8c1a-000000000001/day=2026-11-11/0-<id>.parquet",
            "the object layout is part of the format (spec 03 § 12)"
        );
    }

    #[test]
    fn media_path_matches_the_layout() {
        assert_eq!(
            Objects::media_path(
                project(),
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            "media/018f6b4e-7c1a-7c1a-8c1a-000000000001/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "the object layout is part of the format (spec 03 § 12)"
        );
    }

    #[tokio::test]
    async fn rejected_keys_and_unrelated_files_stay_put() {
        let dir = tempfile::tempdir().unwrap();
        let objects = Objects::new(&local_config(dir.path()), Hooks::default())
            .await
            .unwrap();
        for key in ["", "/abs", "a/../b", "a//b", r"a\b", "a/./b"] {
            let error = objects
                .put_durable(key, Bytes::from_static(b"x"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ObjectError::InvalidPath),
                "{key:?} -> {error}"
            );
        }
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        objects
            .put_durable(&key, Bytes::from_static(b"keep"))
            .await
            .unwrap();
        let root = dir.path().join("objects");
        let parent = root.join(&key).parent().unwrap().to_path_buf();
        std::fs::write(parent.join(".keep"), b"keep").unwrap();
        std::fs::write(parent.join("note.tmp-1"), b"not-hidden").unwrap();
        assert_eq!(objects.remove_temp_files().unwrap(), 0);
        assert_eq!(std::fs::read(parent.join(".keep")).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(parent.join("note.tmp-1")).unwrap(),
            b"not-hidden"
        );
        assert_eq!(std::fs::read(root.join(&key)).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn a_deleted_object_stays_deleted_after_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = Lossy::enable(&data, &dir.path().join("shadow"));
        let objects = Objects::new(&local_config(&data), Hooks::default())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        objects
            .put_durable(&key, Bytes::from_static(b"gone"))
            .await
            .unwrap();
        objects.delete(&key).await.unwrap();
        drop(objects);

        lossy.crash_with(CrashMode::DropAll, 0);
        assert!(
            !data.join("objects").join(&key).exists(),
            "a deleted object came back after a lossy crash"
        );
    }

    #[tokio::test]
    async fn an_object_in_a_directory_left_unsynced_survives_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = Lossy::enable(&data, &dir.path().join("shadow"));
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        // The state a killed process leaves: every directory of the chain
        // created, none of them synced.
        let root = data.join("objects");
        crate::fs::create_dir(&root).unwrap();
        let mut current = root.clone();
        for segment in std::path::Path::new(&key).parent().unwrap().components() {
            current.push(segment);
            crate::fs::create_dir(&current).unwrap();
        }

        let objects = Objects::new(&local_config(&data), Hooks::default())
            .await
            .unwrap();
        objects.remove_temp_files().unwrap();
        objects
            .put_durable(&key, Bytes::from_static(b"survives"))
            .await
            .unwrap();
        drop(objects);

        lossy.crash_with(CrashMode::DropAll, 0);
        assert_eq!(
            std::fs::read(root.join(&key)).expect("the object is lost with its directory"),
            b"survives"
        );
    }

    #[tokio::test]
    async fn deleting_a_key_whose_directory_is_missing_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let objects = Objects::new(&local_config(dir.path()), Hooks::default())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        assert_eq!(objects.delete_len(&key).await.unwrap(), 0);
        objects.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn an_existing_outside_object_root_keeps_its_entry_after_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = tempfile::tempdir().unwrap();
        let lossy = Lossy::enable(dir.path(), &shadow.path().join("shadow"));
        let data = dir.path().join("data");
        let root = dir.path().join("obj");
        // A killed process left the object root created and never synced in
        // its parent, which is outside `data`. Every directory of the chain
        // exists, so only the root's own entry can still be unsynced.
        crate::fs::create_dir(&root).unwrap();
        let mut config = local_config(&data);
        config.object_store = Some(ObjectStoreUrl::Local(root.clone()));

        let objects = Objects::new(&config, Hooks::default()).await.unwrap();
        drop(objects);

        lossy.crash_with(CrashMode::DropAll, 0);
        assert!(
            root.is_dir(),
            "the object root's entry in its parent was lost after a lossy crash"
        );
    }

    #[tokio::test]
    async fn an_outside_object_root_under_an_unsynced_ancestor_survives_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = tempfile::tempdir().unwrap();
        let lossy = Lossy::enable(dir.path(), &shadow.path().join("shadow"));
        let data = dir.path().join("data");
        // A killed process created `outside` and never synced its entry in
        // `dir`. The object root does not exist yet, so `create_dir_chain`
        // creates it below `outside`, the nearest existing ancestor.
        let outside = dir.path().join("outside");
        crate::fs::create_dir(&outside).unwrap();
        let root = outside.join("obj");
        let mut config = local_config(&data);
        config.object_store = Some(ObjectStoreUrl::Local(root.clone()));

        let objects = Objects::new(&config, Hooks::default()).await.unwrap();
        drop(objects);

        lossy.crash_with(CrashMode::DropAll, 0);
        assert!(
            root.is_dir(),
            "the object root was lost after a lossy crash: its unsynced ancestor's entry was never synced"
        );
    }

    #[test]
    fn a_one_component_relative_object_root_is_found_under_the_current_directory() {
        // Only the path is looked up: the process's current directory is
        // never changed. The missing component's parent is `""`, which must
        // map to `.`, and `.` exists.
        let relative = std::path::Path::new("no-such-object-root-for-r2-3");
        assert_eq!(
            super::nearest_existing_ancestor(relative).unwrap(),
            std::path::PathBuf::from(".")
        );
    }

    #[tokio::test]
    async fn a_failed_object_fsync_refuses_later_writes_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::default();
        let objects = Objects::new(&local_config(dir.path()), hooks.clone())
            .await
            .unwrap();
        let (kept, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let (lost, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let (refused, _) = Objects::table_file_path(FileTable::Scores, project(), day(), 1);
        objects
            .put_durable(&kept, Bytes::from_static(b"kept"))
            .await
            .unwrap();

        hooks.arm_fail(HookPoint::ObjectSync);
        let error = objects
            .put_durable(&lost, Bytes::from_static(b"lost"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Io { .. }), "{error}");
        assert!(objects.sync_failed());

        let error = objects
            .put_durable(&refused, Bytes::from_static(b"refused"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::SyncFailed), "{error}");
        let error = objects.delete(&kept).await.unwrap_err();
        assert!(matches!(error, ObjectError::SyncFailed), "{error}");
        let error = objects.delete_len(&kept).await.unwrap_err();
        assert!(matches!(error, ObjectError::SyncFailed), "{error}");
        assert!(matches!(
            objects.create_output(&refused).await,
            Err(ObjectError::SyncFailed)
        ));

        let root = dir.path().join("objects");
        assert!(
            root.join(&kept).is_file(),
            "a refused delete removed the object"
        );
        assert!(!root.join(&refused).exists());
        assert_eq!(
            objects.get_range(&kept, None).await.unwrap(),
            Bytes::from_static(b"kept")
        );
    }

    #[tokio::test]
    async fn a_failed_delete_sync_stops_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = Lossy::enable(&data, &dir.path().join("shadow"));
        let objects = Objects::new(&local_config(&data), Hooks::default())
            .await
            .unwrap();
        let (key, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        objects
            .put_durable(&key, Bytes::from_static(b"gone"))
            .await
            .unwrap();
        let parent = data
            .join("objects")
            .join(&key)
            .parent()
            .unwrap()
            .to_path_buf();
        lossy.fail_next_sync(&parent);

        let error = objects.delete(&key).await.unwrap_err();
        assert!(matches!(error, ObjectError::Io { .. }), "{error}");
        assert!(objects.sync_failed());
        let (other, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        let error = objects
            .put_durable(&other, Bytes::from_static(b"refused"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::SyncFailed), "{error}");
    }

    #[tokio::test]
    async fn a_failed_sync_of_a_new_day_directory_stops_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lossy = Lossy::enable(&data, &dir.path().join("shadow"));
        let objects = Objects::new(&local_config(&data), Hooks::default())
            .await
            .unwrap();
        let (kept, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 0);
        objects
            .put_durable(&kept, Bytes::from_static(b"kept"))
            .await
            .unwrap();

        // The next day's directory does not exist yet, so its creation syncs
        // the project directory. A retry would find the directory and skip
        // that sync, so the failure has to stop the store (D-R-6).
        let next_day = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        let (fresh, _) = Objects::table_file_path(FileTable::Spans, project(), next_day, 0);
        let project_dir = data
            .join("objects")
            .join(&fresh)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        lossy.fail_next_sync(&project_dir);

        let error = objects
            .put_durable(&fresh, Bytes::from_static(b"lost"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::Io { .. }), "{error}");
        assert!(
            objects.sync_failed(),
            "a failed sync of a new directory did not stop the store"
        );
        let (other, _) = Objects::table_file_path(FileTable::Spans, project(), day(), 1);
        let error = objects
            .put_durable(&other, Bytes::from_static(b"refused"))
            .await
            .unwrap_err();
        assert!(matches!(error, ObjectError::SyncFailed), "{error}");
    }
}
