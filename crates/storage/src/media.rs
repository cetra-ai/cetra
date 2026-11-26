//! Media blobs: the collection of unreferenced ones (spec 03 §9, D-A1-6).
//!
//! [`crate::Storage::put_media`] and [`crate::Storage::get_media`] live on
//! `Storage`; this module holds the blob types and [`MediaGc`], the collection
//! the background GC task, the compaction task and
//! [`crate::Storage::apply_deletions`] share.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cetra_column::{
    Batch, CancelToken, Column, DataType, FlatData, GlobalPool, ScalarValue, Selection,
};
use cetra_core::ids::ProjectId;
use cetra_core::schema::spans;
use cetra_query::exec::streaming::UnionSource;
use cetra_query::exec::{
    ControlFlow, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, PushSink, ScanSource, ScanSpec,
    SpillManager, run_pipeline,
};
use cetra_query::{EvalContext, QueryError};
use chrono::NaiveDate;

use crate::catalog::{Committer, FileMeta, FileTable, now_ns};
use crate::engine::mem_source::MemtableSource;
use crate::engine::parquet_source::ParquetSource;
use crate::error::{EngineError, MediaError};
use crate::hooks::{HookPoint, Hooks};
use crate::objects::Objects;
use crate::read_state::Published;
use crate::verify::{Suspects, VerifyQueue};
use crate::wal::Table;

/// One extracted media blob (spec 02 § Media).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaBlob {
    /// SHA-256 of `bytes`. [`crate::Storage::put_media`] checks it.
    pub sha256: [u8; 32],
    /// MIME type, ASCII, 1 to 255 bytes.
    pub mime_type: String,
    /// The bytes. Never empty.
    pub bytes: Bytes,
    /// The newest `start_time` (ns) of a span that references the blob; [`crate::Storage::get_media`]
    /// hides the blob before it (D-P-21).
    pub newest_span_start: i64,
}

/// [`crate::Storage::get_media`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRead {
    /// The stored MIME type.
    pub mime_type: String,
    /// Size of the whole blob, whatever the range.
    pub size: u64,
    /// The requested bytes.
    pub bytes: Bytes,
}

/// What one media collection did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaReport {
    /// Objects deleted, with their rows.
    pub deleted: u64,
    /// Bytes of the deleted objects.
    pub bytes: u64,
}

/// Blobs are kept this long after their last `put_media`, so a span appended
/// right after its blob is never raced (spec 03 §9).
pub(crate) const MEDIA_GRACE: Duration = Duration::from_secs(60 * 60);

/// Interval of the background media collection.
pub(crate) const MEDIA_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// What a collection needs. Cloned into the GC and compaction tasks.
#[derive(Clone)]
pub(crate) struct MediaGc {
    pub(crate) committer: Arc<Committer>,
    pub(crate) objects: Arc<Objects>,
    pub(crate) published: Arc<Published>,
    pub(crate) hooks: Hooks,
    /// The suspects of the live-set query go here (spec 03 §10).
    pub(crate) verify: Arc<VerifyQueue>,
    /// Persistent native query runtime (`workers` + `metadata_cache`).
    pub(crate) qe_runtime: Arc<crate::QeRuntime>,
    /// Tokio runtime handle for async object-store reads from worker threads.
    pub(crate) runtime_handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for MediaGc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MediaGc")
    }
}

impl MediaGc {
    /// Constructs a [`MediaGc`] with native engine handles (accepting legacy args until M7).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        committer: Arc<Committer>,
        objects: Arc<Objects>,
        published: Arc<Published>,
        _target_partitions: NonZeroUsize,
        _lookback: i64,
        hooks: Hooks,
        verify: Arc<VerifyQueue>,
        qe_runtime: Arc<crate::QeRuntime>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            committer,
            objects,
            published,
            hooks,
            verify,
            qe_runtime,
            runtime_handle,
        }
    }
    /// One project's unreferenced blobs (spec 03 §9): every hash whose row is
    /// stale (`last_seen_at` more than [`MEDIA_GRACE`] before `now`) and that
    /// no `spans` row of the project references. Rows hidden by tombstones
    /// don't count: the live set goes through the same read path as queries.
    ///
    /// Errors are logged by the callers as `error = error.label()`: the error
    /// text may name an object path, which holds the blob's hash.
    pub(crate) async fn collect(
        &self,
        project: ProjectId,
        now: i64,
    ) -> Result<MediaReport, MediaError> {
        // With deletion off (D-N-34) nothing is collected: no transaction, no staged row, no delete.
        if !self.objects.deletes_enabled() {
            return Ok(MediaReport::default());
        }
        let grace = i64::try_from(MEDIA_GRACE.as_nanos()).unwrap_or(i64::MAX);
        let before = now.saturating_sub(grace);
        let live = self.live_set(project).await?;
        let mut report = MediaReport::default();
        for sha in self.committer.stale_media(project, before).await? {
            if live.contains(&sha) {
                continue;
            }
            let path = Objects::media_path(project, &sha);
            // The gate comes before any committer call (N M2, D-N-18).
            let deleted = {
                let _gate = self.committer.object_gc_gate.read().await;
                self.committer
                    .collect_media(project, &sha, before, &self.hooks, || {
                        self.objects.delete_len(&path)
                    })
                    .await?
            };
            let Some(bytes) = deleted else {
                continue;
            };
            report.deleted += 1;
            report.bytes = report.bytes.saturating_add(bytes);
        }
        if report.deleted > 0 {
            tracing::info!(
                project = %project,
                deleted = report.deleted,
                bytes = report.bytes,
                "media collected"
            );
        }
        Ok(report)
    }

    /// Every project with media rows, summed. A dropped project is skipped:
    /// [`Self::finish_drop`] deletes its media (spec 03 §7). A project whose
    /// collection fails is logged (`error = error.label()`) and skipped, and
    /// retried on the next run; the others are still collected. Only reading
    /// the project list can fail the whole call.
    pub(crate) async fn collect_all(&self, now: i64) -> Result<MediaReport, MediaError> {
        let state = self.published.load();
        let mut report = MediaReport::default();
        for project in self.committer.media_projects().await? {
            if state.projects.is_dropped(project) {
                continue;
            }
            match self.collect(project, now).await {
                Ok(one) => {
                    report.deleted += one.deleted;
                    report.bytes = report.bytes.saturating_add(one.bytes);
                }
                Err(error) => {
                    // The label only: the error names an object path, which
                    // holds the blob's hash (spec 03 §9).
                    tracing::warn!(
                        project = %project,
                        error = error.label(),
                        "media collection failed"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Deletes a dropped project's media, then marks the drop finished (spec
    /// 03 §7). [`crate::Storage::drop_project`] calls it right after its
    /// commit; the GC task calls it again for a drop whose media step failed
    /// or was interrupted by a crash.
    ///
    /// Idempotent: a project with no media row left only gets its `dropped`
    /// state. Errors are logged by the callers as `error = error.label()`.
    pub(crate) async fn finish_drop(&self, project: ProjectId) -> Result<(), MediaError> {
        // Held to the end: a catalog snapshot cannot run while media is deleted (N M2, D-N-18).
        let _gate = self.committer.object_gc_gate.read().await;
        for sha in self.committer.media_of(project).await? {
            let path = Objects::media_path(project, &sha);
            self.hooks.pause(HookPoint::MediaBeforeDelete);
            if self.hooks.fail(HookPoint::MediaBeforeDelete).is_some() {
                return Err(MediaError::Fault);
            }
            // With deletion off (D-N-34) this returns before `finish_drop`, so the drop stays
            // `dropping` and its blobs are deleted by a later server.
            self.objects.delete(&path).await?;
            self.committer.delete_media_row(project, &sha).await?;
        }
        self.committer.finish_drop(project).await?;
        Ok(())
    }

    /// Finishes every half-finished drop (spec 03 §7): the GC task calls this
    /// at open and on each tick, and a project that fails again is retried on
    /// the next tick.
    pub(crate) async fn finish_drops(&self) {
        let projects = match self.committer.dropping_projects().await {
            Ok(projects) => projects,
            Err(error) => {
                tracing::warn!("reading half-finished project drops failed");
                tracing::debug!(%error, "reading half-finished project drops failed");
                return;
            }
        };
        for project in projects {
            if let Err(error) = self.finish_drop(project).await {
                // The label only: the error names an object path, which holds
                // the blob's hash (spec 03 §9).
                tracing::warn!(
                    project = %project,
                    error = error.label(),
                    "finishing a project drop failed"
                );
            }
        }
    }

    /// Every hash the project's `spans` rows reference, from one published
    /// snapshot through the native query sources (spec 03 §9).
    async fn live_set(&self, project: ProjectId) -> Result<HashSet<String>, MediaError> {
        let state = self.published.load();
        if state.projects.is_dropped(project) {
            return Ok(HashSet::new());
        }

        let deletions = state.tombstones.for_project(project);
        let keep_from = state.projects.keep_from(project);
        let file_keep_from = keep_from.unwrap_or(NaiveDate::MIN);
        let files: Vec<Arc<FileMeta>> = state
            .catalog
            .files(FileTable::Spans, project, file_keep_from..=NaiveDate::MAX)
            .cloned()
            .collect();

        let schema = FileTable::Spans.column_schema();
        let media_refs_idx = schema.index_of(spans::MEDIA_REFS).ok_or_else(|| {
            query_err_to_media(&QueryError::internal("spans schema missing media_refs"))
        })?;

        let suspects = Suspects::new(Arc::clone(&self.verify));
        let parquet_source = ParquetSource::new(
            self.objects.store(),
            FileTable::Spans,
            Arc::clone(&schema),
            files,
            Arc::clone(&deletions),
            Arc::clone(&self.qe_runtime.metadata_cache),
            suspects,
            self.runtime_handle.clone(),
        );
        let mem_source = MemtableSource::new(
            Table::Spans,
            project,
            keep_from,
            Arc::clone(&state.spans),
            deletions,
        )
        .map_err(|e| query_err_to_media(&e))?;
        let union = UnionSource::new(schema, vec![Arc::new(parquet_source), Arc::new(mem_source)])
            .map_err(|e| query_err_to_media(&e))?;

        let workers = self
            .qe_runtime
            .workers()
            .map_err(|e| query_err_to_media(&e))?;
        let target_partitions = self.qe_runtime.target_partitions.max(1);
        let cancel = CancelToken::new();
        let cancel_guard = CancelOnDrop(cancel.clone());

        let live = tokio::task::spawn_blocking(move || -> Result<HashSet<String>, QueryError> {
            let pool = GlobalPool::standalone(usize::MAX);
            let spill = Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0));
            let exec_ctx = ExecContext {
                pool,
                eval: EvalContext::new(now_ns(), cancel),
                spill,
                workers,
                batch_size: DEFAULT_BATCH_SIZE,
                target_partitions,
                metrics: ExecMetrics::new(),
            };
            let spec = ScanSpec {
                projection: Some(vec![media_refs_idx]),
                ..ScanSpec::default()
            };
            let partitions = union.open_partitions(&spec, &exec_ctx)?;
            if partitions.is_empty() {
                return Ok(HashSet::new());
            }
            let sinks = run_pipeline(
                &exec_ctx,
                partitions,
                |_| Ok(Vec::new()),
                |_| Ok(MediaRefSink::default()),
            )?;
            let mut merged = HashSet::new();
            for sink in sinks {
                if merged.is_empty() {
                    merged = sink.live;
                } else {
                    merged.extend(sink.live);
                }
            }
            Ok(merged)
        })
        .await
        .map_err(|_| query_err_to_media(&QueryError::internal("media GC scan task failed")))?
        .map_err(|e| query_err_to_media(&e))?;

        drop(cancel_guard);
        Ok(live)
    }
}

struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn query_err_to_media(err: &QueryError) -> MediaError {
    MediaError::Query(EngineError::from_query(err))
}

#[derive(Default)]
struct MediaRefSink {
    live: HashSet<String>,
}

impl PushSink for MediaRefSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        insert_media_refs(&batch, &mut self.live)?;
        Ok(ControlFlow::Continue)
    }
}

/// Every string of `batch`'s single `media_refs` list column into `live`.
fn insert_media_refs(batch: &Batch, live: &mut HashSet<String>) -> Result<(), QueryError> {
    if batch.is_empty() {
        return Ok(());
    }
    let not_a_list = || QueryError::internal("the media_refs column is not a string list");
    let col = batch.column(0).ok_or_else(not_a_list)?;

    if let Column::Const { value, len } = col {
        if *len < batch.raw_len() {
            return Err(not_a_list());
        }
        return match value {
            ScalarValue::Null(DataType::List(inner))
                if matches!(inner.as_ref(), DataType::Utf8) =>
            {
                Ok(())
            }
            ScalarValue::Null(DataType::Null) => Ok(()),
            ScalarValue::List { item_type, values } if *item_type == DataType::Utf8 => {
                for sv in values {
                    match sv {
                        ScalarValue::Null(_) => {}
                        ScalarValue::Utf8(s) => {
                            if !live.contains(s.as_str()) {
                                live.insert(s.clone());
                            }
                        }
                        _ => return Err(not_a_list()),
                    }
                }
                Ok(())
            }
            _ => Err(not_a_list()),
        };
    }

    let mut visit = |phys_row: usize| -> Result<(), QueryError> {
        let (flat, flat_row) = match col {
            Column::Flat(flat) => {
                if flat.is_null(phys_row) {
                    return Ok(());
                }
                (flat, phys_row)
            }
            Column::Dict(dict) => {
                if dict.is_null(phys_row) {
                    return Ok(());
                }
                let code = *dict.codes().get(phys_row).ok_or_else(not_a_list)? as usize;
                (dict.values(), code)
            }
            Column::Rle(rle) => {
                let run = rle
                    .run_ends()
                    .partition_point(|&end| (end as usize) <= phys_row);
                if rle.values().is_null(run) {
                    return Ok(());
                }
                (rle.values(), run)
            }
            Column::Const { .. } => return Err(not_a_list()),
        };
        match flat.data() {
            FlatData::List {
                item_type,
                offsets,
                values,
            } if *item_type == DataType::Utf8 => {
                let start = *offsets.get(flat_row).ok_or_else(not_a_list)? as usize;
                let end = *offsets.get(flat_row + 1).ok_or_else(not_a_list)? as usize;
                insert_utf8_elements(values.as_ref(), start..end, live)
            }
            _ => Err(not_a_list()),
        }
    };

    match batch.selection() {
        Selection::All => {
            for phys_row in 0..batch.raw_len() {
                visit(phys_row)?;
            }
        }
        Selection::Mask(mask) => {
            for phys_row in mask.iter_ones() {
                visit(phys_row)?;
            }
        }
        Selection::Indices(indices) => {
            for &phys_row in indices {
                visit(phys_row as usize)?;
            }
        }
    }
    Ok(())
}

fn insert_utf8_elements(
    values: &Column,
    range: Range<usize>,
    live: &mut HashSet<String>,
) -> Result<(), QueryError> {
    if range.is_empty() {
        return Ok(());
    }
    let not_a_list = || QueryError::internal("the media_refs column is not a string list");
    match values {
        Column::Flat(flat) => match flat.data() {
            FlatData::Utf8(utf8) => {
                for idx in range {
                    if !flat.is_null(idx) {
                        let s = utf8.get(idx).ok_or_else(not_a_list)?;
                        if !live.contains(s) {
                            live.insert(s.to_owned());
                        }
                    }
                }
                Ok(())
            }
            FlatData::Null(len) if range.end <= *len => Ok(()),
            _ => Err(not_a_list()),
        },
        Column::Const { value, len } => {
            if range.end > *len {
                return Err(not_a_list());
            }
            match value {
                ScalarValue::Null(_) => Ok(()),
                ScalarValue::Utf8(s) => {
                    if !live.contains(s.as_str()) {
                        live.insert(s.clone());
                    }
                    Ok(())
                }
                _ => Err(not_a_list()),
            }
        }
        Column::Dict(dict) => match dict.values().data() {
            FlatData::Utf8(utf8) => {
                for idx in range {
                    if !dict.is_null(idx) {
                        let code = *dict.codes().get(idx).ok_or_else(not_a_list)? as usize;
                        let s = utf8.get(code).ok_or_else(not_a_list)?;
                        if !live.contains(s) {
                            live.insert(s.to_owned());
                        }
                    }
                }
                Ok(())
            }
            _ => Err(not_a_list()),
        },
        Column::Rle(rle) => match rle.values().data() {
            FlatData::Utf8(utf8) => {
                for idx in range {
                    if idx >= rle.len() {
                        return Err(not_a_list());
                    }
                    let run = rle.run_ends().partition_point(|&end| (end as usize) <= idx);
                    if !rle.values().is_null(run) {
                        let s = utf8.get(run).ok_or_else(not_a_list)?;
                        if !live.contains(s) {
                            live.insert(s.to_owned());
                        }
                    }
                }
                Ok(())
            }
            _ => Err(not_a_list()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use cetra_core::ids::ProjectId;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::{MediaBlob, MediaError, MediaReport};
    use crate::Storage;
    use crate::catalog::now_ns;
    use crate::compaction::job::tests::{at, project, quiet_compaction};
    use crate::deletion::{Deletion, DeletionScope};
    use crate::hooks::HookPoint;
    use crate::objects::Objects;
    use crate::test_support::{span_row, span_rows};

    /// One hour, the media grace, in ns.
    const HOUR_NS: i64 = 3_600_000_000_000;

    /// The second fixed test project, after `project()` in id order.
    fn other_project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000002".parse().unwrap()
    }

    fn sha256_of(bytes: &Bytes) -> [u8; 32] {
        Sha256::digest(bytes.as_ref()).into()
    }

    fn blob_of(bytes: &[u8], mime_type: &str) -> MediaBlob {
        let bytes = Bytes::copy_from_slice(bytes);
        MediaBlob {
            sha256: sha256_of(&bytes),
            mime_type: mime_type.to_owned(),
            bytes,
            newest_span_start: 0,
        }
    }

    /// The blob's object path under the local object root.
    fn object_file(dir: &Path, project: ProjectId, sha: &[u8; 32]) -> PathBuf {
        dir.join("objects")
            .join(Objects::media_path(project, &hex::encode(sha)))
    }

    /// Files under `objects/media/`, all projects: two directory levels.
    fn media_objects(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(projects) = std::fs::read_dir(dir.join("objects").join("media")) else {
            return found;
        };
        for project in projects.flatten() {
            if let Ok(files) = std::fs::read_dir(project.path()) {
                found.extend(files.flatten().map(|file| file.path()));
            }
        }
        found
    }

    /// `(first_seen_at, last_seen_at)` of the blob's `media` row.
    async fn seen_at(dir: &Path, project: ProjectId, sha: &[u8; 32]) -> (i64, i64) {
        use sqlx::ConnectOptions;
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        sqlx::query_as(
            "SELECT first_seen_at, last_seen_at FROM media WHERE project_id = ? AND sha256 = ?",
        )
        .bind(project.to_string())
        .bind(hex::encode(sha))
        .fetch_one(&mut connection)
        .await
        .unwrap()
    }

    async fn media_rows(dir: &Path) -> i64 {
        use sqlx::ConnectOptions;
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        sqlx::query_scalar("SELECT COUNT(*) FROM media")
            .fetch_one(&mut connection)
            .await
            .unwrap()
    }

    fn config_in(dir: &Path) -> crate::config::StorageConfig {
        crate::compaction::job::tests::config_in(
            dir,
            quiet_compaction(8),
            Duration::from_secs(3_600),
        )
    }

    // 1. A put followed by a get returns the bytes and the MIME type, whole or
    //    as a range.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn put_then_get_round_trips_whole_and_ranged() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"media-bytes-0123456789", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();

        let read = storage
            .get_media(project(), &sha, None, None)
            .await
            .unwrap();
        assert_eq!(read.mime_type, "image/png");
        assert_eq!(read.bytes, Bytes::from_static(b"media-bytes-0123456789"));
        assert_eq!(read.size, 22);
        let object = object_file(dir.path(), project(), &sha);
        assert_eq!(
            std::fs::read(&object).unwrap(),
            b"media-bytes-0123456789".to_vec(),
            "the object is not at {object:?}"
        );

        let ranged = storage
            .get_media(project(), &sha, Some(2..5), None)
            .await
            .unwrap();
        assert_eq!(ranged.bytes, Bytes::from_static(b"dia"));
        assert_eq!(ranged.size, 22, "the range keeps the whole blob's size");
        storage.shutdown().await.unwrap();
    }

    // 2. A wrong hash, a bad MIME type, an empty blob, an unknown hash and a
    //    range outside the blob are refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn invalid_blobs_unknown_hashes_and_bad_ranges_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();

        let mut wrong_hash = blob_of(b"some media", "image/png");
        wrong_hash.sha256 = [1; 32];
        assert!(
            matches!(
                storage.put_media(project(), wrong_hash).await,
                Err(MediaError::HashMismatch)
            ),
            "a wrong hash passed"
        );
        assert!(
            media_objects(dir.path()).is_empty(),
            "an object was written"
        );
        assert_eq!(media_rows(dir.path()).await, 0, "a row was written");

        for mime_type in ["", &"x".repeat(256), "imäge/png"] {
            let blob = blob_of(b"some media", mime_type);
            assert!(
                matches!(
                    storage.put_media(project(), blob).await,
                    Err(MediaError::Invalid {
                        reason: "mime type"
                    })
                ),
                "{mime_type:?} passed"
            );
        }
        assert!(
            matches!(
                storage
                    .put_media(project(), blob_of(b"", "image/png"))
                    .await,
                Err(MediaError::Invalid {
                    reason: "empty blob"
                })
            ),
            "an empty blob passed"
        );
        assert_eq!(media_rows(dir.path()).await, 0);
        assert!(media_objects(dir.path()).is_empty());

        let unknown = [7; 32];
        assert!(
            matches!(
                storage.get_media(project(), &unknown, None, None).await,
                Err(MediaError::NotFound)
            ),
            "an unknown hash was found"
        );

        let blob = blob_of(b"0123456789", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        for range in [Some(5..5), Some(0..11), Some(11..12)] {
            assert!(
                matches!(
                    storage
                        .get_media(project(), &sha, range.clone(), None)
                        .await,
                    Err(MediaError::InvalidRange)
                ),
                "{range:?} passed"
            );
        }
        storage.shutdown().await.unwrap();
    }

    // 3. A second put of a stored hash writes nothing new and raises
    //    `last_seen_at`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_put_touches_only_last_seen_at() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"idempotent-media", "image/png");
        let sha = blob.sha256;
        let first_at = now_ns();
        storage.put_media(project(), blob.clone()).await.unwrap();
        let (first, last) = seen_at(dir.path(), project(), &sha).await;
        assert_eq!(first, last);
        assert!(first >= first_at);

        tokio::time::sleep(Duration::from_millis(5)).await;
        storage.put_media(project(), blob).await.unwrap();
        let (first_again, last_again) = seen_at(dir.path(), project(), &sha).await;
        assert_eq!(first_again, first, "first_seen_at moved");
        assert!(last_again > last, "last_seen_at did not rise");
        assert_eq!(media_rows(dir.path()).await, 1);
        assert_eq!(media_objects(dir.path()).len(), 1);
        assert!(storage.staged_paths().await.unwrap().is_empty());
        storage.shutdown().await.unwrap();
    }

    // 4. Eight concurrent puts of one blob leave one row, one object and no
    //    staged row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_leave_one_row_and_one_object() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(config_in(dir.path())).await.unwrap());
        let blob = blob_of(b"concurrent-media", "image/png");
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let storage = Arc::clone(&storage);
            let blob = blob.clone();
            tasks.push(tokio::spawn(async move {
                storage.put_media(project(), blob).await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(media_rows(dir.path()).await, 1);
        assert_eq!(media_objects(dir.path()).len(), 1);
        assert!(storage.staged_paths().await.unwrap().is_empty());
        Arc::try_unwrap(storage)
            .unwrap_or_else(|_| panic!("storage still shared"))
            .shutdown()
            .await
            .unwrap();
    }

    // 5. A crash between the put and the insert leaves the object staged;
    //    staged-object GC deletes it and its row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fault_after_the_put_leaves_the_object_for_staged_gc() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"crashed-media", "image/png");
        let sha = blob.sha256;
        storage.hooks().arm_fail(HookPoint::MediaAfterPut);
        assert!(
            matches!(
                storage.put_media(project(), blob).await,
                Err(MediaError::Fault)
            ),
            "the fault did not fail the put"
        );
        let path = Objects::media_path(project(), &hex::encode(sha));
        assert!(dir.path().join("objects").join(&path).is_file());
        assert_eq!(storage.staged_paths().await.unwrap(), vec![path.clone()]);
        assert_eq!(media_rows(dir.path()).await, 0);

        storage.collect_staged(Duration::ZERO).await.unwrap();
        assert!(!dir.path().join("objects").join(&path).exists());
        assert!(storage.staged_paths().await.unwrap().is_empty());
        storage.shutdown().await.unwrap();
    }

    // 6. A stale staged row of a stored blob: collection removes only the
    //    staged row; the object, the `media` row and `get_media` stay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_staged_row_of_a_stored_blob_is_removed_without_the_object() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"stored-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let path = Objects::media_path(project(), &hex::encode(sha));
        // What a crashed second put would have left behind.
        storage.committer.stage_media(&path, 1).await.unwrap();

        storage.collect_staged(Duration::ZERO).await.unwrap();
        assert!(
            storage.staged_paths().await.unwrap().is_empty(),
            "the staged row stayed"
        );
        assert!(
            dir.path().join("objects").join(&path).is_file(),
            "the object was deleted"
        );
        assert_eq!(media_rows(dir.path()).await, 1);
        let read = storage
            .get_media(project(), &sha, None, None)
            .await
            .unwrap();
        assert_eq!(read.bytes, Bytes::from_static(b"stored-media"));
        storage.shutdown().await.unwrap();
    }

    // 9a. A blob referenced by a span in a file is kept however stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_blob_referenced_by_a_span_in_a_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let blob = blob_of(b"file-referenced-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let time = at(0, 0, 0);
        let mut span = span_row(1, 0, time, time);
        span.media_refs = vec![hex::encode(sha)];
        storage
            .append_spans(project(), span_rows(&[span]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.catalog_rows(crate::wal::Table::Spans), 1);
        assert_eq!(storage.published_rows(crate::wal::Table::Spans), 0);
        let report = storage.collect_media(now_ns() + 2 * HOUR_NS).await.unwrap();
        assert_eq!(report, MediaReport::default());
        let read = storage
            .get_media(project(), &sha, None, None)
            .await
            .unwrap();
        assert_eq!(read.bytes, Bytes::from_static(b"file-referenced-media"));
        storage.shutdown().await.unwrap();
    }

    // 9b. A blob referenced only by a memtable span is kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_blob_referenced_only_by_a_memtable_span_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"memtable-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let time = at(0, 0, 0);
        let mut span = span_row(1, 0, time, time);
        span.media_refs = vec![hex::encode(sha)];
        storage
            .append_spans(project(), span_rows(&[span]))
            .await
            .unwrap();
        assert_eq!(storage.published_rows(crate::wal::Table::Spans), 1);

        let report = storage.collect_media(now_ns() + 2 * HOUR_NS).await.unwrap();
        assert_eq!(report, MediaReport::default());
        storage
            .get_media(project(), &sha, None, None)
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    // 9c. An unreferenced blob older than the grace is deleted and counted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_unreferenced_blob_is_deleted_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let bytes = b"orphan-media";
        let blob = blob_of(bytes, "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();

        let report = storage.collect_media(now_ns() + 2 * HOUR_NS).await.unwrap();
        assert_eq!(
            report,
            MediaReport {
                deleted: 1,
                bytes: u64::try_from(bytes.len()).unwrap(),
            }
        );
        assert!(!object_file(dir.path(), project(), &sha).exists());
        assert_eq!(media_rows(dir.path()).await, 0);
        assert!(
            matches!(
                storage.get_media(project(), &sha, None, None).await,
                Err(MediaError::NotFound)
            ),
            "the row survived its collection"
        );
        storage.shutdown().await.unwrap();
    }

    // 9d. An unreferenced blob seen within the grace is kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unreferenced_blob_seen_within_the_grace_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"fresh-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();

        let report = storage.collect_media(now_ns()).await.unwrap();
        assert_eq!(report, MediaReport::default());
        assert!(object_file(dir.path(), project(), &sha).is_file());
        assert_eq!(media_rows(dir.path()).await, 1);
        storage.shutdown().await.unwrap();
    }

    // 9e. A project whose collection fails does not block the others: the
    //     error is logged and its blobs retried on the next run. The one-shot
    //     `MediaBeforeDelete` fault hits the first project in id order; it
    //     fires after the row delete committed, so the leftover is a staged
    //     row with the object, which staged-object GC removes (spec 03 §9).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failing_project_does_not_block_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let other = other_project();
        let mine = blob_of(b"failed-project-media", "image/png");
        let theirs = blob_of(b"kept-project-media", "image/png");
        let mine_sha = mine.sha256;
        let theirs_sha = theirs.sha256;
        storage.put_media(project(), mine).await.unwrap();
        storage.put_media(other, theirs).await.unwrap();
        storage.hooks().arm_fail(HookPoint::MediaBeforeDelete);

        let now = now_ns() + 2 * HOUR_NS;
        let report = storage.collect_media(now).await.unwrap();
        assert_eq!(report.deleted, 1, "the other project was not collected");
        assert_eq!(
            report.bytes,
            u64::try_from(b"kept-project-media".len()).unwrap()
        );
        assert!(!object_file(dir.path(), other, &theirs_sha).exists());
        assert!(object_file(dir.path(), project(), &mine_sha).is_file());
        // The fault fired after the failing project's row delete: only a
        // staged row remains of it.
        assert_eq!(media_rows(dir.path()).await, 0);
        let staged = storage.staged_paths().await.unwrap();
        assert_eq!(staged.len(), 1, "{staged:?}");

        // The leftover is collected by staged-object GC, and a later media
        // collection has nothing left to do.
        storage.collect_staged(Duration::ZERO).await.unwrap();
        assert!(!object_file(dir.path(), project(), &mine_sha).exists());
        assert!(storage.staged_paths().await.unwrap().is_empty());
        assert_eq!(
            storage.collect_media(now).await.unwrap(),
            MediaReport::default()
        );
        storage.shutdown().await.unwrap();
    }

    // 10. Applying a deletion collects the media its rows referenced: the pass
    //     runs one collection per applied project at its own `now`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_deletion_collects_the_media_of_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let blob = blob_of(b"deleted-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let time = at(0, 0, 0);
        let mut span = span_row(1, 0, time, time);
        span.media_refs = vec![hex::encode(sha)];
        let trace_id = span.trace_id;
        storage
            .append_spans(project(), span_rows(&[span]))
            .await
            .unwrap();
        let deletion = Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: time + 1,
            scope: DeletionScope::Traces {
                trace_ids: vec![trace_id],
            },
        };
        storage.register_deletion(deletion.clone()).await.unwrap();
        // The shutdown flush drops the covered rows; no file ever holds them.
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        let now = now_ns() + 2 * HOUR_NS;
        let report = storage.apply_deletions(now).await.unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            storage.deletion_status(deletion.id).await.unwrap(),
            crate::deletion::DeletionStatus::Applied { applied_at: now }
        );
        assert!(
            !object_file(dir.path(), project(), &sha).exists(),
            "the blob survived its deletion"
        );
        assert_eq!(media_rows(dir.path()).await, 0);
        assert!(
            matches!(
                storage.get_media(project(), &sha, None, None).await,
                Err(MediaError::NotFound)
            ),
            "the row survived its deletion"
        );
        storage.shutdown().await.unwrap();
    }

    /// `blob_of` whose newest span starts at `start` (ns).
    fn blob_starting(bytes: &[u8], start: i64) -> MediaBlob {
        MediaBlob {
            newest_span_start: start,
            ..blob_of(bytes, "image/png")
        }
    }

    // D-P-21: a blob whose newest span is older than the visible bound is not found; a
    // read without a bound finds it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_blob_is_hidden_before_its_newest_span() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_starting(b"hidden-until-100", 100);
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();

        assert!(matches!(
            storage.get_media(project(), &sha, None, Some(101)).await,
            Err(MediaError::NotFound)
        ));
        assert!(
            storage
                .get_media(project(), &sha, None, Some(100))
                .await
                .is_ok()
        );
        assert!(storage.get_media(project(), &sha, None, None).await.is_ok());
        storage.shutdown().await.unwrap();
    }

    // D-P-21: a later put raises the newest span; a lower one does not lower it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_later_put_raises_the_newest_span() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let sha = blob_starting(b"reused-blob", 100).sha256;
        storage
            .put_media(project(), blob_starting(b"reused-blob", 100))
            .await
            .unwrap();
        assert!(matches!(
            storage.get_media(project(), &sha, None, Some(150)).await,
            Err(MediaError::NotFound)
        ));

        storage
            .put_media(project(), blob_starting(b"reused-blob", 200))
            .await
            .unwrap();
        assert!(
            storage
                .get_media(project(), &sha, None, Some(150))
                .await
                .is_ok()
        );

        storage
            .put_media(project(), blob_starting(b"reused-blob", 50))
            .await
            .unwrap();
        assert!(
            storage
                .get_media(project(), &sha, None, Some(150))
                .await
                .is_ok(),
            "a lower put does not lower the newest span"
        );
        storage.shutdown().await.unwrap();
    }

    // D-P-21: two puts of one new blob that race past `touch_media` both leave their span
    // start; the row keeps the newer one whichever insert won.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_keep_the_newest_span() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let sha = blob_starting(b"raced-blob", 0).sha256;
        let older = storage.put_media(project(), blob_starting(b"raced-blob", 100));
        let newer = storage.put_media(project(), blob_starting(b"raced-blob", 200));
        let (a, b) = tokio::join!(older, newer);
        a.unwrap();
        b.unwrap();

        assert!(
            storage
                .get_media(project(), &sha, None, Some(200))
                .await
                .is_ok()
        );
        assert!(matches!(
            storage.get_media(project(), &sha, None, Some(201)).await,
            Err(MediaError::NotFound)
        ));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_set_short_circuits_on_dropped_project() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(config_in(dir.path())).await.unwrap();
        let blob = blob_of(b"dropped-project-blob", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let time = at(0, 0, 0);
        let mut span = span_row(1, 0, time, time);
        span.media_refs = vec![hex::encode(sha)];
        storage
            .append_spans(project(), span_rows(&[span]))
            .await
            .unwrap();
        assert!(!storage.media.live_set(project()).await.unwrap().is_empty());

        storage
            .published
            .replace(|state| crate::read_state::ReadState {
                projects: Arc::new(state.projects.with_dropped(project())),
                ..state.clone()
            });
        assert!(storage.media.live_set(project()).await.unwrap().is_empty());
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_set_respects_keep_from_retention_in_files_and_memtable() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let old_blob = blob_of(b"old-day-blob", "image/png");
        let new_blob = blob_of(b"new-day-blob", "image/png");
        let mem_old_blob = blob_of(b"mem-old-day-blob", "image/png");
        let old_sha = old_blob.sha256;
        let new_sha = new_blob.sha256;
        let mem_old_sha = mem_old_blob.sha256;
        storage.put_media(project(), old_blob).await.unwrap();
        storage.put_media(project(), new_blob).await.unwrap();
        storage.put_media(project(), mem_old_blob).await.unwrap();

        let day1 = at(0, 0, 0);
        let day2 = day1 + 86_400_000_000_000;
        let mut s1 = span_row(1, 0, day1, day1);
        s1.media_refs = vec![hex::encode(old_sha)];
        let mut s2 = span_row(2, 0, day2, day2);
        s2.media_refs = vec![hex::encode(new_sha)];
        storage
            .append_spans(project(), span_rows(&[s1, s2]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        // Add a late memtable span on day1 as well.
        let mut s3 = span_row(3, 0, day1, day1);
        s3.media_refs = vec![hex::encode(mem_old_sha)];
        storage
            .append_spans(project(), span_rows(&[s3]))
            .await
            .unwrap();

        let keep_day = chrono::DateTime::from_timestamp_nanos(day2).date_naive();
        storage.apply_retention(project(), keep_day).await.unwrap();

        let live = storage.media.live_set(project()).await.unwrap();
        assert!(!live.contains(&hex::encode(old_sha)));
        assert!(!live.contains(&hex::encode(mem_old_sha)));
        assert!(live.contains(&hex::encode(new_sha)));

        let report = storage.collect_media(now_ns() + 2 * HOUR_NS).await.unwrap();
        assert_eq!(report.deleted, 2);
        assert!(
            storage
                .get_media(project(), &new_sha, None, None)
                .await
                .is_ok()
        );
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_set_respects_pending_tombstones_across_files_and_memtable() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let file_blob = blob_of(b"file-tombstoned-blob", "image/png");
        let mem_blob = blob_of(b"mem-tombstoned-blob", "image/png");
        let kept_blob = blob_of(b"kept-blob", "image/png");
        let file_sha = file_blob.sha256;
        let mem_sha = mem_blob.sha256;
        let kept_sha = kept_blob.sha256;
        storage.put_media(project(), file_blob).await.unwrap();
        storage.put_media(project(), mem_blob).await.unwrap();
        storage.put_media(project(), kept_blob).await.unwrap();

        let time = at(0, 0, 0);
        let mut file_span = span_row(1, 0, time, time);
        file_span.media_refs = vec![hex::encode(file_sha)];
        let file_trace = file_span.trace_id;
        let mut kept_span = span_row(2, 0, time, time);
        kept_span.media_refs = vec![hex::encode(kept_sha)];
        storage
            .append_spans(project(), span_rows(&[file_span, kept_span]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        let mut mem_span = span_row(3, 0, time, time);
        mem_span.media_refs = vec![hex::encode(mem_sha)];
        let mem_trace = mem_span.trace_id;
        storage
            .append_spans(project(), span_rows(&[mem_span]))
            .await
            .unwrap();

        storage
            .register_deletion(Deletion {
                id: Uuid::now_v7(),
                project: project(),
                cutoff: time + 1,
                scope: DeletionScope::Traces {
                    trace_ids: vec![file_trace, mem_trace],
                },
            })
            .await
            .unwrap();

        let live = storage.media.live_set(project()).await.unwrap();
        assert!(!live.contains(&hex::encode(file_sha)));
        assert!(!live.contains(&hex::encode(mem_sha)));
        assert!(live.contains(&hex::encode(kept_sha)));
        storage.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_set_reports_corrupt_parquet_file_as_suspect() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        let storage = Storage::open(config.clone()).await.unwrap();
        let blob = blob_of(b"corrupt-file-media", "image/png");
        let sha = blob.sha256;
        storage.put_media(project(), blob).await.unwrap();
        let time = at(0, 0, 0);
        let mut span = span_row(1, 0, time, time);
        span.media_refs = vec![hex::encode(sha)];
        storage
            .append_spans(project(), span_rows(&[span]))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();

        let storage = Storage::open(config).await.unwrap();
        let file = storage.catalog_files(crate::catalog::FileTable::Spans)[0].clone();
        let obj_path = dir.path().join("objects").join(&file.path);
        let mut bytes = std::fs::read(&obj_path).unwrap();
        let len = bytes.len();
        bytes[len - 4..].fill(0xFF);
        std::fs::write(&obj_path, bytes).unwrap();

        let err = storage
            .media
            .live_set(project())
            .await
            .expect_err("corrupt file must fail live_set");
        assert!(matches!(err, MediaError::Query(_)));
        assert_eq!(storage.verify_queue().take(10), vec![file.id]);
        storage.shutdown().await.unwrap();
    }

    #[test]
    fn insert_media_refs_handles_all_encodings_and_selections() {
        use std::collections::HashSet;

        use cetra_column::{
            Batch, Bitmap, Column, DataType, DictColumn, Field, FlatColumn, FlatData, RleColumn,
            ScalarValue, Schema, Selection, Utf8Data,
        };

        let list_dt = DataType::List(Box::new(DataType::Utf8));
        let schema = Schema::try_new_arc(vec![Field::new("media_refs", list_dt, true)]).unwrap();

        // 1. Flat list with Dict child Utf8 + Selection::Mask
        let dict_child_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).unwrap()),
            None,
        )
        .unwrap();
        let dict_child = Column::Dict(
            DictColumn::try_new(
                dict_child_vals,
                vec![0, 1, 0],
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let flat_list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2, 3, 3],
                    values: Box::new(dict_child),
                },
                Some(Bitmap::from_bools(&[true, true, false])),
            )
            .unwrap(),
        );
        let b1 = Batch::try_new(
            Arc::clone(&schema),
            vec![flat_list],
            3,
            Selection::Mask(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let mut live = HashSet::new();
        super::insert_media_refs(&b1, &mut live).unwrap();
        assert_eq!(live, HashSet::from(["a".to_owned()]));

        // 2. Const list
        let const_list = Column::Const {
            value: ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("c1".to_owned()),
                    ScalarValue::Null(DataType::Utf8),
                    ScalarValue::Utf8("c2".to_owned()),
                ],
            },
            len: 2,
        };
        let b2 = Batch::try_new_all(Arc::clone(&schema), vec![const_list], 2).unwrap();
        live.clear();
        super::insert_media_refs(&b2, &mut live).unwrap();
        assert_eq!(live, HashSet::from(["c1".to_owned(), "c2".to_owned()]));

        // 3. Dict list with Rle child Utf8 + Selection::Indices
        let rle_child_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["r1", "r2"]).unwrap()),
            None,
        )
        .unwrap();
        let rle_child = Column::Rle(RleColumn::try_new(rle_child_vals, vec![2, 3]).unwrap());
        let dict_list_vals = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 1, 3],
                values: Box::new(rle_child),
            },
            None,
        )
        .unwrap();
        let dict_list = Column::Dict(
            DictColumn::try_new(
                dict_list_vals,
                vec![0, 1, 0],
                Some(Bitmap::from_bools(&[true, true, false])),
            )
            .unwrap(),
        );
        let b3 = Batch::try_new(
            Arc::clone(&schema),
            vec![dict_list],
            3,
            Selection::Indices(vec![1, 2]),
        )
        .unwrap();
        live.clear();
        super::insert_media_refs(&b3, &mut live).unwrap();
        assert_eq!(live, HashSet::from(["r1".to_owned(), "r2".to_owned()]));

        // 4. Rle list with Const child Utf8
        let const_child = Column::Const {
            value: ScalarValue::Utf8("k".to_owned()),
            len: 2,
        };
        let rle_list_vals = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 2, 2],
                values: Box::new(const_child),
            },
            Some(Bitmap::from_bools(&[true, false])),
        )
        .unwrap();
        let rle_list = Column::Rle(RleColumn::try_new(rle_list_vals, vec![2, 4]).unwrap());
        let b4 = Batch::try_new_all(schema, vec![rle_list], 4).unwrap();
        live.clear();
        super::insert_media_refs(&b4, &mut live).unwrap();
        assert_eq!(live, HashSet::from(["k".to_owned()]));
    }
}
