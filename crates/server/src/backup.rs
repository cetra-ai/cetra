//! Online backups of a running server (N M2, D-N-17–D-N-19, D-N-24).
//!
//! [`run`] follows § Consistency of the N M2 design: flush both tables, copy
//! `meta.db`, pin and copy the catalog, link or list the objects, copy the key,
//! and write `manifest.json` last. B7 exposes it on the control socket.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use llmobs_storage::CopyReport;
use llmobs_storage::config::ObjectStoreUrl;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::crypto;
use crate::http::AppState;

/// How long the flush before a backup may take (§ Consistency, step 3).
const FLUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Mode of the backup directory and of the files the backup creates in it.
const DIR_MODE: u32 = 0o700;
const KEY_MODE: u32 = 0o600;
/// `manifest.json`'s `format`.
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
    /// The target path is not absolute.
    #[error("the target must be an absolute path")]
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
    Flush(#[from] llmobs_storage::FlushNowError),
    /// `VACUUM INTO` of `meta.db` failed.
    #[error("meta.db copy: {0}")]
    Meta(sqlx::Error),
    /// The catalog snapshot failed.
    #[error("catalog snapshot: {0}")]
    Snapshot(#[from] llmobs_storage::SnapshotError),
    /// Linking or copying the objects failed.
    #[error("objects: {0}")]
    Objects(#[from] llmobs_storage::CopyObjectsError),
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
/// directory. A failure after the call creates `to` removes what was written.
pub(crate) async fn run(
    state: &AppState,
    to: &Path,
    meta_only: bool,
) -> Result<BackupReport, BackupError> {
    let started = Instant::now();
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
    match write(state, &target.path, mode, started).await {
        Ok(report) => {
            tracing::info!(
                mode = ?report.mode,
                objects = report.objects,
                object_bytes = report.object_bytes,
                duration_ms = report.duration_ms,
                "online backup written"
            );
            Ok(report)
        }
        Err(error) => {
            undo(&target).await;
            Err(error)
        }
    }
}

/// The backup directory, and whether this call created it.
struct Target {
    path: PathBuf,
    created: bool,
}

/// Checks `to` and makes it an empty directory with mode `0700` (§ Consistency, step 3).
async fn prepare_target(state: &AppState, to: &Path) -> Result<Target, BackupError> {
    if !to.is_absolute() {
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

/// Removes what a failed backup wrote. Unlinking a hard link leaves the data
/// directory's file alone.
async fn undo(target: &Target) {
    let result = if target.created {
        tokio::fs::remove_dir_all(&target.path).await
    } else {
        remove_entries(&target.path).await
    };
    if let Err(error) = result {
        tracing::warn!(kind = ?error.kind(), "could not remove a failed backup");
    }
}

async fn remove_entries(path: &Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
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
) -> Result<BackupReport, BackupError> {
    state.storage.flush_now(FLUSH_TIMEOUT).await?;
    let created = Utc::now();
    copy_meta(state, dir).await?;

    let snapshot = state
        .storage
        .snapshot_catalog(&dir.join("catalog.db"))
        .await?;
    let flushed_seq = snapshot.flushed_seq;
    let copy = match mode {
        BackupMode::Full => {
            state
                .storage
                .copy_objects(&snapshot, &dir.join("objects"))
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

    let key = copy_key(state, dir).await?;
    let mut files = vec![
        file_record(dir, "meta.db").await?,
        file_record(dir, "catalog.db").await?,
    ];
    if key == "file" {
        files.push(file_record(dir, "secret.key").await?);
    }
    let created_at = rfc3339(created);
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
    });
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
        "llmobs_version": env!("CARGO_PKG_VERSION"),
        "created_at": rfc3339(parts.created),
        "mode": parts.mode,
        "object_store": parts.object_store,
        "encryption_key": parts.encryption_key,
        "flushed_seq": {"spans": parts.flushed_seq[0], "scores": parts.flushed_seq[1]},
        "gc_grace_secs": parts.gc_grace.as_secs(),
        "restorable_until": restorable_until,
        "files": files,
        "objects": {"count": parts.objects_count, "bytes": parts.objects_bytes},
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
    use datafusion::prelude::{SessionConfig, SessionContext};
    use llmobs_core::batch::spans_to_record_batch;
    use llmobs_core::ids::ProjectId;
    use llmobs_core::span::Span;
    use llmobs_storage::{QueryOptions, Storage};

    use super::*;
    use crate::config::Secret;
    use crate::crypto::{encode_key, random_key};
    use crate::serve::{Running, bind};
    use crate::test_support::{config, dataset_span};

    /// Appends `count` example spans to `project`.
    async fn append(running: &Running, project: ProjectId, count: usize) {
        let spans: Vec<Span> = (0..count)
            .map(|index| {
                let mut span = dataset_span(index);
                span.project_id = project;
                span
            })
            .collect();
        let batch = spans_to_record_batch(&spans).unwrap();
        running
            .state()
            .storage
            .append_spans(project, batch)
            .await
            .unwrap();
    }

    /// How many span rows `project` reads back.
    async fn stored_rows(storage: &Storage, project: ProjectId) -> usize {
        let query = storage
            .query(project, QueryOptions::default())
            .await
            .unwrap();
        let ctx = SessionContext::new_with_config_rt(SessionConfig::new(), query.runtime);
        ctx.register_table("raw_spans", query.spans).unwrap();
        let batches = ctx
            .sql("SELECT span_id FROM raw_spans")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        batches.iter().map(|batch| batch.num_rows()).sum()
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
        assert_eq!(stored_rows(&again.state().storage, project).await, 3);
        again.shutdown().await.unwrap();
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
        });
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["llmobs_version"], env!("CARGO_PKG_VERSION"));
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
        });
        assert_eq!(manifest["mode"], "full");
        assert!(manifest["restorable_until"].is_null());
        assert_eq!(manifest["encryption_key"], "environment");
        assert_eq!(manifest["object_store"], "local");
        assert_eq!(manifest["gc_grace_secs"], 300);
    }
}
