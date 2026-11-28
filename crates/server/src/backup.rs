//! Online backups of a running server (N M2, D-N-17–D-N-19, D-N-24).
//!
//! [`run`] follows § Consistency of the N M2 design: flush both tables, copy
//! `meta.db`, pin and copy the catalog, link or list the objects, copy the key,
//! and write `manifest.json` last. B7 exposes it on the control socket.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cetra_storage::config::ObjectStoreUrl;
use cetra_storage::{CopyCancel, CopyReport};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions as _, Connection as _};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::Instrument as _;

use crate::crypto;
use crate::http::AppState;
use crate::observability::spans;

/// How long the flush before a backup may take (§ Consistency, step 3).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Mode of the backup directory and of the files the backup creates in it.
const DIR_MODE: u32 = 0o700;
const KEY_MODE: u32 = 0o600;
/// `manifest.json`'s `format` (D-R-37). Within 1, keys are only added; a removed, renamed or
/// retyped key, or a changed meaning, is 2. Nothing reads the manifest in the MVP: a newer
/// backup is refused by the database checks at restore.
const FORMAT: u32 = 1;

/// What a finished backup reports (also the socket answer, B7).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BackupReport {
    /// The backup directory, as the server resolved it.
    pub dir: PathBuf,
    /// `full` or `meta-only`.
    pub mode: BackupMode,
    /// RFC 3339, nanoseconds, UTC: the same value the manifest records.
    pub created_at: String,
    /// Objects the snapshot lists.
    pub objects: u64,
    /// Their total size.
    pub object_bytes: u64,
    /// Objects hard-linked into the backup.
    pub linked: u64,
    /// Objects copied because a hard link was impossible.
    pub copied: u64,
    /// Milliseconds from the start of the call to the end of the manifest.
    pub duration_ms: u64,
}

/// Whether the backup holds the objects (`Full`) or only the metadata.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackupMode {
    Full,
    MetaOnly,
}

/// Why a backup failed. [`BackupError::code`] is the stable string the socket returns.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Another backup holds the slot.
    #[error("another backup is running")]
    Busy,
    /// The target path is not absolute, or has a `..` component.
    #[error("the target must be an absolute path without `..`")]
    RelativeTarget,
    /// The target exists and is not an empty directory.
    #[error("the target directory is not empty")]
    TargetNotEmpty,
    /// The target is inside the data directory or a local object store.
    #[error("the target directory is inside the data directory")]
    TargetInsideDataDir,
    /// `meta_only` was asked of a local-mode server.
    #[error(
        "--meta-only needs S3-compatible object storage; a local-mode backup must include its objects"
    )]
    MetaOnlyNeedsS3,
    /// The flush before the backup failed.
    #[error("flush: {0}")]
    Flush(#[from] cetra_storage::FlushNowError),
    /// `VACUUM INTO` of `meta.db` failed.
    #[error("meta.db copy: {0}")]
    Meta(sqlx::Error),
    /// The catalog snapshot failed.
    #[error("catalog snapshot: {0}")]
    Snapshot(#[from] cetra_storage::SnapshotError),
    /// Linking or copying the objects failed.
    #[error("objects: {0}")]
    Objects(#[from] cetra_storage::CopyObjectsError),
    /// Writing the backup failed.
    #[error("writing the backup: {0}")]
    Io(#[from] io::Error),
}

impl BackupError {
    /// The stable code the socket returns, in variant order.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::RelativeTarget => "bad_request",
            Self::TargetNotEmpty => "target_not_empty",
            Self::TargetInsideDataDir => "target_inside_data_dir",
            Self::MetaOnlyNeedsS3 => "meta_only_needs_s3",
            Self::Flush(_) => "flush_failed",
            Self::Meta(_) => "meta_copy_failed",
            Self::Snapshot(_) => "snapshot_failed",
            Self::Objects(_) => "objects_failed",
            Self::Io(_) => "io",
        }
    }
}

/// Takes an online backup into `to` (§ Consistency of N M2's design).
///
/// `to` must be absolute and absent or an empty directory, outside the data
/// directory. A backup that does not finish, by an error or because this future is
/// dropped, removes what it wrote (see [`Partial`]). Runs in a `backup` span.
///
/// The backup runs in a task of its own, which is never aborted: dropping a future
/// does not stop the file writes and SQLite calls it already started on other
/// threads, so removing the directory from a drop would race them. Dropping this
/// future sets the cancel flag instead. The task checks it before each step (and the
/// object copy before each object), stops, and removes the directory once nothing
/// writes into it any more; then it frees the backup slot. A caller that must see
/// the directory gone, such as shutdown, waits for the slot.
pub(crate) async fn run(
    state: &AppState,
    to: &Path,
    meta_only: bool,
) -> Result<BackupReport, BackupError> {
    let span = spans::backup();
    let result = take_backup(state, to, meta_only)
        .instrument(span.clone())
        .await;
    match &result {
        Ok(report) => {
            let mode = match report.mode {
                BackupMode::Full => "full",
                BackupMode::MetaOnly => "meta-only",
            };
            span.record(spans::MODE, mode);
            span.record(spans::OBJECTS, spans::count(report.objects));
            span.record(spans::BYTES, spans::count(report.object_bytes));
            span.record(spans::OUTCOME, "ok");
        }
        Err(error) => {
            span.record(spans::OUTCOME, error.code());
        }
    }
    result
}

/// Takes an online backup into `to` (§ Consistency of N M2's design).
///
/// `to` must be absolute and absent or an empty directory, outside the data
/// directory. A backup that does not finish, by an error or because this future is
/// dropped, removes what it wrote (see [`Partial`]). The backup runs in a task of
/// its own, never aborted: dropping this future sets its cancel flag, and the task
/// stops and removes the directory once nothing writes into it any more.
async fn take_backup(
    state: &AppState,
    to: &Path,
    meta_only: bool,
) -> Result<BackupReport, BackupError> {
    let cancel: CopyCancel = Arc::new(Mutex::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let task = tokio::spawn({
        let (state, to) = (state.clone(), to.to_path_buf());
        async move { take(&state, &to, meta_only, &cancel).await }
    });
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Only a runtime shutdown cancels the task.
        Err(_cancelled) => Err(BackupError::Io(io::Error::other("the backup task stopped"))),
    }
}

/// Sets the backup's cancel flag when [`take_backup`]'s future is dropped.
struct CancelOnDrop(CopyCancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }
}

/// The body of [`take_backup`], in its own task.
async fn take(
    state: &AppState,
    to: &Path,
    meta_only: bool,
    cancel: &CopyCancel,
) -> Result<BackupReport, BackupError> {
    let started = Instant::now();
    // Dropped after `partial`, so the slot frees only once a failed backup is removed.
    let _slot = state
        .backup_slot
        .try_lock()
        .map_err(|_| BackupError::Busy)?;
    let s3 = matches!(
        state.config.storage.object_store_url(),
        ObjectStoreUrl::S3 { .. }
    );
    let mode = match (s3, meta_only) {
        (true, _) => BackupMode::MetaOnly,
        (false, true) => return Err(BackupError::MetaOnlyNeedsS3),
        (false, false) => BackupMode::Full,
    };
    let target = prepare_target(state, to).await?;
    let mut partial = Partial {
        target,
        armed: true,
    };
    let report = write(state, &partial.target.path, mode, started, cancel).await?;
    // Under the lock: a cancel that comes first removes the finished backup, and one
    // that comes after keeps it.
    let _held = not_cancelled(cancel)?;
    partial.armed = false;
    tracing::info!(
        mode = ?report.mode,
        objects = report.objects,
        object_bytes = report.object_bytes,
        duration_ms = report.duration_ms,
        "online backup written"
    );
    Ok(report)
}

/// The backup directory, and whether this call created it.
struct Target {
    path: PathBuf,
    created: bool,
}

/// A backup's directory until the backup succeeds. Armed from the moment the target
/// is claimed, it removes what the backup wrote when dropped: after an error,
/// including a cancel. [`take`] has awaited every step it started by then, so
/// nothing still writes into the directory. Only a runtime shutdown, which drops the
/// task mid-step, can leave a step running.
struct Partial {
    target: Target,
    armed: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.armed {
            undo(&self.target);
        }
    }
}

/// Locks the cancel flag and returns the guard, or an error when [`run`]'s caller
/// gave up. Nobody receives that error: it only stops [`take`].
fn not_cancelled(cancel: &CopyCancel) -> Result<MutexGuard<'_, bool>, BackupError> {
    let guard = cancel.lock().unwrap_or_else(PoisonError::into_inner);
    if *guard {
        Err(BackupError::Objects(
            cetra_storage::CopyObjectsError::Cancelled,
        ))
    } else {
        Ok(guard)
    }
}

/// Stops [`take`] before its next step when [`run`]'s caller gave up.
fn checkpoint(cancel: &CopyCancel) -> Result<(), BackupError> {
    not_cancelled(cancel).map(drop)
}

/// Checks `to` and makes it an empty directory with mode `0700` (§ Consistency, step 3).
async fn prepare_target(state: &AppState, to: &Path) -> Result<Target, BackupError> {
    if !to.is_absolute() || to.components().any(|part| part == Component::ParentDir) {
        return Err(BackupError::RelativeTarget);
    }
    let target = resolve(to).await?;
    let data_dir = resolve(&state.config.data_dir).await?;
    if target.starts_with(&data_dir) {
        return Err(BackupError::TargetInsideDataDir);
    }
    if let ObjectStoreUrl::Local(root) = state.config.storage.object_store_url()
        && target.starts_with(resolve(&root).await?)
    {
        return Err(BackupError::TargetInsideDataDir);
    }
    match tokio::fs::metadata(&target).await {
        Ok(metadata) => {
            if !metadata.is_dir() || !is_empty_dir(&target).await? {
                return Err(BackupError::TargetNotEmpty);
            }
            tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(DIR_MODE)).await?;
            Ok(Target {
                path: target,
                created: false,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::DirBuilder::new()
                .mode(DIR_MODE)
                .create(&target)
                .await?;
            Ok(Target {
                path: target,
                created: true,
            })
        }
        Err(error) => Err(error.into()),
    }
}

/// `path` with symlinks resolved. A path that does not exist resolves through its
/// nearest existing ancestor, and the rest is appended.
async fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        match tokio::fs::canonicalize(existing).await {
            Ok(mut resolved) => {
                for part in rest.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match (existing.file_name(), existing.parent()) {
                    (Some(name), Some(parent)) => {
                        rest.push(name);
                        existing = parent;
                    }
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn is_empty_dir(path: &Path) -> io::Result<bool> {
    let mut entries = tokio::fs::read_dir(path).await?;
    Ok(entries.next_entry().await?.is_none())
}

/// Removes what a failed or cancelled backup wrote. It uses `std::fs`, because it
/// runs in `Drop`. Unlinking a hard link leaves the data directory's file alone.
fn undo(target: &Target) {
    let result = if target.created {
        std::fs::remove_dir_all(&target.path)
    } else {
        remove_entries(&target.path)
    };
    if let Err(error) = result {
        tracing::warn!(kind = ?error.kind(), "could not remove a failed backup");
    }
}

fn remove_entries(path: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Steps 4 to 9 of § Consistency, in `dir`.
async fn write(
    state: &AppState,
    dir: &Path,
    mode: BackupMode,
    started: Instant,
    cancel: &CopyCancel,
) -> Result<BackupReport, BackupError> {
    checkpoint(cancel)?;
    state.storage.flush_now(FLUSH_TIMEOUT).await?;
    let created = Utc::now();
    checkpoint(cancel)?;
    copy_meta(state, dir).await?;

    checkpoint(cancel)?;
    let snapshot = state
        .storage
        .snapshot_catalog(&dir.join("catalog.db"))
        .await?;
    let flushed_seq = snapshot.flushed_seq;
    let copy = match mode {
        BackupMode::Full => {
            state
                .storage
                .copy_objects(&snapshot, &dir.join("objects"), cancel.clone())
                .await?
        }
        BackupMode::MetaOnly => CopyReport {
            count: u64::try_from(snapshot.objects.len()).unwrap_or(u64::MAX),
            bytes: snapshot
                .objects
                .iter()
                .fold(0_u64, |sum, object| sum.saturating_add(object.size_bytes)),
            linked: 0,
            copied: 0,
        },
    };
    // The objects are linked or listed now. Holding the pin longer only delays deletions.
    drop(snapshot);

    checkpoint(cancel)?;
    let key = copy_key(state, dir).await?;
    let mut files = vec![
        file_record(dir, "meta.db").await?,
        file_record(dir, "catalog.db").await?,
    ];
    if key == "file" {
        files.push(file_record(dir, "secret.key").await?);
    }
    let created_at = rfc3339(created);
    checkpoint(cancel)?;
    let formats = read_formats(dir).await?;
    let manifest = manifest(&ManifestParts {
        created,
        mode,
        object_store: match mode {
            BackupMode::Full => "local",
            BackupMode::MetaOnly => "s3",
        },
        encryption_key: key,
        flushed_seq,
        gc_grace: state.config.storage.gc.grace,
        files: &files,
        objects_count: copy.count,
        objects_bytes: copy.bytes,
        formats,
    });
    checkpoint(cancel)?;
    write_manifest(dir, &manifest).await?;

    Ok(BackupReport {
        dir: dir.to_path_buf(),
        mode,
        created_at,
        objects: copy.count,
        object_bytes: copy.bytes,
        linked: copy.linked,
        copied: copy.copied,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// `meta.db` by `VACUUM INTO` on a reader connection. It is copied before
/// `catalog.db` (§ Consistency, step 4).
async fn copy_meta(state: &AppState, dir: &Path) -> Result<(), BackupError> {
    let path = dir.join("meta.db");
    let text = path_text(&path)?;
    let mut connection = state
        .meta
        .reader()
        .acquire()
        .await
        .map_err(BackupError::Meta)?;
    sqlx::query("VACUUM INTO ?")
        .bind(text)
        .execute(&mut *connection)
        .await
        .map_err(BackupError::Meta)?;
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, BackupError> {
    path.to_str().ok_or_else(|| {
        BackupError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the backup path is not UTF-8",
        ))
    })
}

/// Copies `secret.key` into `dir` when the key is a file, and says where the key
/// is kept: `file` or `environment` (§ Consistency, step 7).
async fn copy_key(state: &AppState, dir: &Path) -> Result<&'static str, BackupError> {
    if state.config.encryption_key.is_some() {
        return Ok("environment");
    }
    match tokio::fs::read(crypto::key_path(&state.config.data_dir)).await {
        Ok(bytes) => {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(KEY_MODE)
                .open(dir.join("secret.key"))
                .await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            Ok("file")
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok("environment"),
        Err(error) => Err(error.into()),
    }
}

/// One file the manifest lists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRecord {
    path: &'static str,
    bytes: u64,
    sha256: String,
}

/// Syncs `dir/name` and hashes it (§ Backup layout and manifest).
async fn file_record(dir: &Path, name: &'static str) -> Result<FileRecord, BackupError> {
    let mut file = tokio::fs::File::open(dir.join(name)).await?;
    file.sync_all().await?;
    let bytes = file.metadata().await?.len();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(FileRecord {
        path: name,
        bytes,
        sha256: hex::encode(hasher.finalize().as_slice()),
    })
}

/// The database versions of the backup, read from its own copies (D-R-37).
struct Formats {
    /// `PRAGMA user_version` of the backup's `meta.db` (META_FORMAT_VERSION).
    meta_format: i64,
    /// Highest `_sqlx_migrations.version` of the backup's `meta.db`.
    meta_schema: i64,
    /// `PRAGMA user_version` of the backup's `catalog.db` (storage FORMAT_VERSION, D-R-26).
    storage_format: i64,
    /// Highest `_sqlx_migrations.version` of the backup's `catalog.db`.
    catalog_schema: i64,
}

/// Reads the versions of `dir/meta.db` and `dir/catalog.db`. Both copies are opened
/// read-only, so reading them writes nothing.
async fn read_formats(dir: &Path) -> Result<Formats, BackupError> {
    let (meta_format, meta_schema) = read_versions(&dir.join("meta.db")).await?;
    let (storage_format, catalog_schema) = read_versions(&dir.join("catalog.db")).await?;
    Ok(Formats {
        meta_format,
        meta_schema,
        storage_format,
        catalog_schema,
    })
}

/// `PRAGMA user_version` and the highest migration version of one database file.
async fn read_versions(path: &Path) -> Result<(i64, i64), BackupError> {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .map_err(|error| sql_error(&error))?;
    let versions = async {
        let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await?;
        let schema: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&mut connection)
                .await?;
        Ok::<_, sqlx::Error>((user_version, schema))
    }
    .await;
    let closed = connection.close().await;
    let versions = versions.map_err(|error| sql_error(&error))?;
    closed.map_err(|error| sql_error(&error))?;
    Ok(versions)
}

/// A failed read of a copy. The text is sqlx's, which names no row values.
fn sql_error(error: &sqlx::Error) -> BackupError {
    BackupError::Io(io::Error::other(error.to_string()))
}

/// What `manifest.json` records (D-N-19).
struct ManifestParts<'a> {
    created: DateTime<Utc>,
    mode: BackupMode,
    /// `local` or `s3`.
    object_store: &'static str,
    /// `file` or `environment`.
    encryption_key: &'static str,
    flushed_seq: [u64; 2],
    gc_grace: Duration,
    files: &'a [FileRecord],
    objects_count: u64,
    objects_bytes: u64,
    formats: Formats,
}

/// The manifest of § Backup layout and manifest, as JSON.
fn manifest(parts: &ManifestParts<'_>) -> Value {
    let restorable_until = match parts.mode {
        BackupMode::Full => Value::Null,
        BackupMode::MetaOnly => TimeDelta::from_std(parts.gc_grace)
            .ok()
            .and_then(|grace| parts.created.checked_add_signed(grace))
            .map_or(Value::Null, |until| Value::String(rfc3339(until))),
    };
    let files: Vec<Value> = parts
        .files
        .iter()
        .map(|file| json!({"path": file.path, "bytes": file.bytes, "sha256": file.sha256}))
        .collect();
    json!({
        "format": FORMAT,
        "cetra_version": env!("CARGO_PKG_VERSION"),
        "created_at": rfc3339(parts.created),
        "mode": parts.mode,
        "object_store": parts.object_store,
        "encryption_key": parts.encryption_key,
        "flushed_seq": {"spans": parts.flushed_seq[0], "scores": parts.flushed_seq[1]},
        "gc_grace_secs": parts.gc_grace.as_secs(),
        "restorable_until": restorable_until,
        "files": files,
        "objects": {"count": parts.objects_count, "bytes": parts.objects_bytes},
        "meta_format": parts.formats.meta_format,
        "meta_schema": parts.formats.meta_schema,
        "storage_format": parts.formats.storage_format,
        "catalog_schema": parts.formats.catalog_schema,
    })
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Writes `manifest.json` through a temporary file, then syncs the directory.
async fn write_manifest(dir: &Path, manifest: &Value) -> Result<(), BackupError> {
    let text = serde_json::to_vec_pretty(manifest).map_err(io::Error::from)?;
    let temporary = dir.join("manifest.json.tmp");
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(&text).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, dir.join("manifest.json")).await?;
    tokio::fs::File::open(dir).await?.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use cetra_core::batch::spans_to_batch;
    use cetra_core::ids::ProjectId;
    use cetra_core::span::Span;

    use super::*;
    use crate::config::Secret;
    use crate::crypto::{encode_key, random_key};
    use crate::observability::otlp::receiver::{int_attribute, string_attribute};
    use crate::observability::subscriber::capture_spans;
    use crate::serve::{Running, bind};
    use crate::test_support::{TestApp, config, dataset_span, raw_span_count};

    /// Appends `count` example spans to `project`.
    async fn append(running: &Running, project: ProjectId, count: usize) {
        let spans: Vec<Span> = (0..count)
            .map(|index| {
                let mut span = dataset_span(index);
                span.project_id = project;
                span
            })
            .collect();
        let batch = spans_to_batch(&spans).unwrap();
        running
            .state()
            .storage
            .append_spans(project, batch)
            .await
            .unwrap();
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn count_files(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    count_files(&entry.path())
                } else {
                    1
                }
            })
            .sum()
    }

    fn sha256_hex(path: &Path) -> String {
        hex::encode(Sha256::digest(std::fs::read(path).unwrap()).as_slice())
    }

    fn read_manifest(target: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(target.join("manifest.json")).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_full_backup_has_every_file_and_a_manifest() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let project = ProjectId::new_v7();
        append(&running, project, 3).await;
        let target = root.path().join("backup");
        let report = run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        for name in ["meta.db", "catalog.db", "secret.key", "manifest.json"] {
            assert!(target.join(name).is_file(), "{name} is missing");
        }
        assert!(report.objects > 0);
        assert_eq!(count_files(&target.join("objects")), report.objects);
        assert_eq!(report.mode, BackupMode::Full);

        let manifest = read_manifest(&target);
        assert_eq!(manifest["mode"], "full");
        assert!(manifest["restorable_until"].is_null());
        assert_eq!(manifest["format"], FORMAT);
        assert_eq!(manifest["objects"]["count"], report.objects);
        assert_eq!(manifest["created_at"], report.created_at);
        let files = manifest["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        for file in files {
            let name = file["path"].as_str().unwrap();
            let path = target.join(name);
            assert_eq!(file["sha256"], sha256_hex(&path), "{name}");
            assert_eq!(
                file["bytes"],
                std::fs::metadata(&path).unwrap().len(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn the_crash_spool_is_not_backed_up() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let spool = data.join("telemetry").join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(
            spool.join("crash-00000000000000000001-0000000001.json"),
            "{}",
        )
        .unwrap();
        let project = ProjectId::new_v7();
        append(&running, project, 3).await;
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        assert!(!target.join("telemetry").exists());
        let mut names: Vec<String> = std::fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "catalog.db",
                "manifest.json",
                "meta.db",
                "objects",
                "secret.key"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_backup_runs_in_its_span() {
        let app = TestApp::new().await;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("b");
        let (result, captured) = capture_spans(run(&app.state, &target, false)).await;
        let report = result.unwrap();
        assert_eq!(report.mode, BackupMode::Full);

        let found = captured.named("backup");
        assert_eq!(found.len(), 1);
        let attributes = &found[0].attributes;
        assert_eq!(
            string_attribute(attributes, "mode"),
            Some("full".to_owned())
        );
        assert!(int_attribute(attributes, "objects").is_some_and(|count| count >= 0));
        assert!(int_attribute(attributes, "bytes").is_some_and(|count| count >= 0));
        assert_eq!(
            string_attribute(attributes, "outcome"),
            Some("ok".to_owned())
        );

        let text = captured.text();
        assert!(!text.contains(&*target.to_string_lossy()), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_backup_records_its_code() {
        let app = TestApp::new().await;
        let (result, captured) = capture_spans(run(&app.state, Path::new("relative"), false)).await;
        assert!(
            matches!(result, Err(BackupError::RelativeTarget)),
            "the relative target is refused"
        );

        let found = captured.named("backup");
        assert_eq!(found.len(), 1);
        assert_eq!(
            string_attribute(&found[0].attributes, "outcome"),
            Some("bad_request".to_owned())
        );
    }

    #[tokio::test]
    async fn a_full_backup_restores_with_its_data() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let project = ProjectId::new_v7();
        append(&running, project, 3).await;
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        let restored = root.path().join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        for name in ["meta.db", "catalog.db", "secret.key"] {
            std::fs::copy(target.join(name), restored.join(name)).unwrap();
        }
        copy_tree(&target.join("objects"), &restored.join("objects"));

        let again = bind(config(&restored), None).await.unwrap();
        assert_eq!(raw_span_count(&again.state().storage, project).await, 3);
        again.shutdown().await.unwrap();
    }

    /// `(user_version, highest migration)` of a live database file.
    async fn live_versions(path: &Path) -> (i64, i64) {
        let mut connection = SqliteConnectOptions::new()
            .filename(path)
            .connect()
            .await
            .unwrap();
        let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        let schema: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        connection.close().await.unwrap();
        (user_version, schema)
    }

    #[tokio::test]
    async fn the_manifest_records_the_database_versions() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        let manifest = read_manifest(&target);
        assert_eq!(
            manifest["meta_format"],
            i64::from(crate::meta::META_FORMAT_VERSION)
        );
        assert_eq!(manifest["meta_schema"], crate::meta::expected_version());
        let (storage_format, catalog_schema) = live_versions(&data.join("catalog.db")).await;
        assert_eq!(manifest["storage_format"], storage_format);
        assert_eq!(manifest["catalog_schema"], catalog_schema);
    }

    #[tokio::test]
    async fn the_manifest_keys_are_pinned() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        let manifest = read_manifest(&target);
        let mut keys: Vec<&str> = manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "catalog_schema",
                "cetra_version",
                "created_at",
                "encryption_key",
                "files",
                "flushed_seq",
                "format",
                "gc_grace_secs",
                "meta_format",
                "meta_schema",
                "mode",
                "object_store",
                "objects",
                "restorable_until",
                "storage_format",
            ],
            "manifest.json format 1 only gains keys (D-R-37): a removed, renamed or retyped key bumps FORMAT"
        );
        assert!(manifest["format"].is_u64());
        assert!(manifest["cetra_version"].is_string());
        assert!(manifest["created_at"].is_string());
        assert!(manifest["mode"].is_string());
        assert!(manifest["object_store"].is_string());
        assert!(manifest["encryption_key"].is_string());
        assert!(manifest["flushed_seq"].is_object());
        assert!(manifest["gc_grace_secs"].is_u64());
        assert!(manifest["restorable_until"].is_null());
        assert!(manifest["files"].is_array());
        assert!(manifest["objects"].is_object());
        for key in [
            "meta_format",
            "meta_schema",
            "storage_format",
            "catalog_schema",
        ] {
            assert!(manifest[key].is_i64(), "{key} is not an integer");
        }
    }

    #[tokio::test]
    async fn a_newer_backup_is_refused_by_the_first_restore_command() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let project = ProjectId::new_v7();
        append(&running, project, 3).await;
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        // No journal mode is set, so the copy stays in rollback mode, as it was written.
        let mut connection = SqliteConnectOptions::new()
            .filename(target.join("meta.db"))
            .connect()
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();

        // Restore as docs/self-hosting/backup.md § Restore step 3 says.
        let restored = root.path().join("restored");
        std::fs::create_dir_all(&restored).unwrap();
        for name in ["meta.db", "catalog.db", "secret.key"] {
            std::fs::copy(target.join(name), restored.join(name)).unwrap();
        }
        copy_tree(&target.join("objects"), &restored.join("objects"));
        let before = crate::meta::test_digest::dir_digest(&restored);

        let error = crate::admin::gc_orphans(&config(&restored), Duration::ZERO, false, false)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, crate::admin::AdminError::Database(text) if text.contains("format version 2")),
            "{error}"
        );
        assert_eq!(
            crate::meta::test_digest::dir_digest(&restored),
            before,
            "the refused restore changed the data directory"
        );
    }

    #[tokio::test]
    async fn a_second_backup_while_one_runs_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let state = running.state();
        let held = state.backup_slot.lock().await;
        let error = run(state, &root.path().join("backup"), false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "busy");
        drop(held);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_target_must_be_empty_and_outside_the_data_dir() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let state = running.state();

        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("keep.txt"), b"keep").unwrap();
        let error = run(state, &occupied, false).await.unwrap_err();
        assert!(matches!(error, BackupError::TargetNotEmpty), "{error}");
        assert!(occupied.join("keep.txt").is_file());

        let inside = data.join("x");
        let error = run(state, &inside, false).await.unwrap_err();
        assert!(matches!(error, BackupError::TargetInsideDataDir), "{error}");
        assert!(!inside.exists());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_relative_target_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let error = run(running.state(), Path::new("rel"), false)
            .await
            .unwrap_err();
        assert!(matches!(error, BackupError::RelativeTarget), "{error}");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_parent_dir_component_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let target = Path::new("/tmp/x/../y");
        let error = run(running.state(), target, false).await.unwrap_err();
        assert!(matches!(error, BackupError::RelativeTarget), "{error}");
        assert_eq!(error.code(), "bad_request");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_symlink_into_the_data_dir_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        // A directory inside the data directory, reached through a link outside it.
        let inside = data.join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&inside, &link).unwrap();

        let error = run(running.state(), &link, false).await.unwrap_err();
        assert!(matches!(error, BackupError::TargetInsideDataDir), "{error}");
        assert!(
            std::fs::read_dir(&inside).unwrap().next().is_none(),
            "a refused backup wrote inside the data directory"
        );
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_backup_directory_is_0700_and_the_key_0600() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target), 0o700);
        assert_eq!(mode(&target.join("secret.key")), 0o600);
    }

    #[tokio::test]
    async fn an_existing_empty_target_is_set_to_0700() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let target = root.path().join("backup");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test]
    async fn a_target_inside_a_local_object_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let objects = root.path().join("objects-root");
        std::fs::create_dir_all(&objects).unwrap();
        let mut cfg = config(&root.path().join("data"));
        cfg.storage.object_store = Some(ObjectStoreUrl::Local(objects.clone()));
        let running = bind(cfg, None).await.unwrap();
        let target = objects.join("backup");

        let error = run(running.state(), &target, false).await.unwrap_err();
        assert!(matches!(error, BackupError::TargetInsideDataDir), "{error}");
        assert!(!target.exists());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_backup_removes_the_directory_it_created() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let state = running.state();
        state.storage.shutdown().await.unwrap();
        let target = root.path().join("backup");
        let error = run(state, &target, false).await.unwrap_err();
        assert!(matches!(error, BackupError::Flush(_)), "{error}");
        assert!(!target.exists());
        // The storage is already shut down, so this call's error is only logged.
        let _ = running.shutdown().await;
    }

    #[test]
    fn a_dropped_partial_backup_removes_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("backup");
        std::fs::create_dir_all(path.join("objects")).unwrap();
        std::fs::write(path.join("meta.db"), b"partial").unwrap();

        drop(Partial {
            target: Target {
                path: path.clone(),
                created: true,
            },
            armed: true,
        });
        assert!(!path.exists(), "the cancelled backup left its directory");
    }

    #[test]
    fn a_disarmed_partial_backup_keeps_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("backup");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("manifest.json"), b"{}").unwrap();

        drop(Partial {
            target: Target {
                path: path.clone(),
                created: true,
            },
            armed: false,
        });
        assert!(path.join("manifest.json").is_file());
    }

    /// Each step checks the flag: a backup cancelled before it starts writes nothing
    /// and frees the slot.
    #[tokio::test]
    async fn a_backup_cancelled_before_its_first_step_removes_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let state = running.state();
        let target = root.path().join("backup");
        let cancel: CopyCancel = Arc::new(Mutex::new(true));

        let error = take(state, &target, false, &cancel).await.unwrap_err();
        assert!(
            matches!(
                error,
                BackupError::Objects(cetra_storage::CopyObjectsError::Cancelled)
            ),
            "{error}"
        );
        assert!(!target.exists(), "the cancelled backup left its directory");
        assert!(
            state.backup_slot.try_lock().is_ok(),
            "the slot is still held"
        );
        running.shutdown().await.unwrap();
    }

    /// Aborting a running backup leaves no target content, and a retry to the same
    /// target succeeds. The abort waits, bounded, until `objects/` exists: the copy
    /// creates it after both `VACUUM INTO` copies have finished, so the abort lands
    /// in the object copy or after it. If the backup finishes before the abort, the
    /// test fails rather than passing without exercising the cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_backup_leaves_no_target_and_a_retry_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let project = ProjectId::new_v7();
        append(&running, project, 3).await;
        let state = running.state().clone();
        let target = root.path().join("backup");

        let task = tokio::spawn({
            let (state, target) = (state.clone(), target.clone());
            async move { run(&state, &target, false).await.map(|_| ()) }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !target.join("objects").exists() && !task.is_finished() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        task.abort();
        let Err(aborted) = task.await else {
            panic!("the backup finished before the abort");
        };
        assert!(aborted.is_cancelled());
        // The backup's own task removes the directory, then frees the slot.
        drop(state.backup_slot.lock().await);
        assert!(!target.exists(), "the cancelled backup left its directory");

        let report = run(&state, &target, false).await.unwrap();
        assert_eq!(report.mode, BackupMode::Full);
        assert!(target.join("manifest.json").is_file());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn meta_only_is_refused_in_local_mode() {
        let root = tempfile::tempdir().unwrap();
        let running = bind(config(&root.path().join("data")), None).await.unwrap();
        let target = root.path().join("backup");
        let error = run(running.state(), &target, true).await.unwrap_err();
        assert!(matches!(error, BackupError::MetaOnlyNeedsS3), "{error}");
        assert!(!target.exists());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_key_is_not_copied_when_it_comes_from_the_environment() {
        let root = tempfile::tempdir().unwrap();
        let mut from_env = config(&root.path().join("data"));
        from_env.encryption_key = Some(Secret::new(encode_key(&random_key().unwrap())));
        let running = bind(from_env, None).await.unwrap();
        let target = root.path().join("backup");
        run(running.state(), &target, false).await.unwrap();
        running.shutdown().await.unwrap();

        assert!(!target.join("secret.key").exists());
        let manifest = read_manifest(&target);
        assert_eq!(manifest["encryption_key"], "environment");
        let names: Vec<&str> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["meta.db", "catalog.db"]);
    }

    fn sample_files() -> Vec<FileRecord> {
        vec![FileRecord {
            path: "meta.db",
            bytes: 10,
            sha256: "ab".repeat(32),
        }]
    }

    fn sample_formats() -> Formats {
        Formats {
            meta_format: 1,
            meta_schema: 15,
            storage_format: 1,
            catalog_schema: 4,
        }
    }

    #[test]
    fn a_meta_only_manifest_records_its_restorable_window() {
        let files = sample_files();
        let manifest = manifest(&ManifestParts {
            created: "2026-11-14T21:04:05.123456789Z".parse().unwrap(),
            mode: BackupMode::MetaOnly,
            object_store: "s3",
            encryption_key: "file",
            flushed_seq: [1234, 56],
            gc_grace: Duration::from_secs(8 * 86_400),
            files: &files,
            objects_count: 812,
            objects_bytes: 3_221_225_472,
            formats: sample_formats(),
        });
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["meta_format"], 1);
        assert_eq!(manifest["meta_schema"], 15);
        assert_eq!(manifest["storage_format"], 1);
        assert_eq!(manifest["catalog_schema"], 4);
        assert_eq!(manifest["cetra_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["created_at"], "2026-11-14T21:04:05.123456789Z");
        assert_eq!(manifest["mode"], "meta-only");
        assert_eq!(manifest["object_store"], "s3");
        assert_eq!(manifest["encryption_key"], "file");
        assert_eq!(
            manifest["flushed_seq"],
            json!({"spans": 1234, "scores": 56})
        );
        assert_eq!(manifest["gc_grace_secs"], 691_200);
        assert_eq!(
            manifest["restorable_until"],
            "2026-11-22T21:04:05.123456789Z"
        );
        assert_eq!(
            manifest["files"],
            json!([{"path": "meta.db", "bytes": 10, "sha256": "ab".repeat(32)}])
        );
        assert_eq!(
            manifest["objects"],
            json!({"count": 812, "bytes": 3_221_225_472_u64})
        );
    }

    #[test]
    fn a_full_manifest_from_the_environment_has_no_restorable_until() {
        let files = sample_files();
        let manifest = manifest(&ManifestParts {
            created: "2026-11-14T21:04:05Z".parse().unwrap(),
            mode: BackupMode::Full,
            object_store: "local",
            encryption_key: "environment",
            flushed_seq: [0, 0],
            gc_grace: Duration::from_secs(300),
            files: &files,
            objects_count: 0,
            objects_bytes: 0,
            formats: sample_formats(),
        });
        assert_eq!(manifest["mode"], "full");
        assert!(manifest["restorable_until"].is_null());
        assert_eq!(manifest["encryption_key"], "environment");
        assert_eq!(manifest["object_store"], "local");
        assert_eq!(manifest["gc_grace_secs"], 300);
    }
}
