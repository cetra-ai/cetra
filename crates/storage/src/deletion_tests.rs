//! End-to-end deletion tests: tombstones in reads, `register_deletion`,
//! `deletion_status` (M5.2, spec 03 §7).

use std::path::Path;
use std::time::{Duration, Instant};

use bytesize::ByteSize;
use cetra_core::ids::ProjectId;
use cetra_core::score::ScoreTarget;
use uuid::Uuid;

use crate::QueryOptions;
use crate::compaction::job::CompactionReport;
use crate::compaction::rewrite::DeletionReport;
use crate::config::{FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig};
use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::error::{CatalogError, DeletionError};
use crate::hooks::HookPoint;
use crate::test_support::{
    object_rows, rows, score_row, score_rows, span_row, span_rows, trace_ids_of,
};
use crate::views::QueryContext;
use crate::wal::Table;

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
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

fn traces_deletion(traces: &[u16], cutoff: i64) -> Deletion {
    Deletion {
        id: Uuid::now_v7(),
        project: project(),
        cutoff,
        scope: DeletionScope::Traces {
            trace_ids: traces
                .iter()
                .map(|trace| span_row(*trace, 0, 0, 0).trace_id)
                .collect(),
        },
    }
}

/// One fresh session over the latest published state.
async fn context(storage: &crate::Storage) -> QueryContext {
    QueryContext::new(
        storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap(),
        None,
    )
    .unwrap()
}

/// The first integer of the first row of `sql`.
async fn scalar(storage: &crate::Storage, sql: &str) -> i64 {
    let result = rows(&context(storage).await, sql).await;
    result[0].values().next().unwrap().as_i64().unwrap()
}

async fn span_count(storage: &crate::Storage, trace: u16) -> i64 {
    let hex = span_row(trace, 0, 0, 0).trace_id.to_hex();
    scalar(
        storage,
        &format!("SELECT count(*) FROM spans WHERE trace_id = '{hex}'"),
    )
    .await
}

async fn trace_count(storage: &crate::Storage, trace: u16) -> i64 {
    let hex = span_row(trace, 0, 0, 0).trace_id.to_hex();
    scalar(
        storage,
        &format!("SELECT count(*) FROM traces WHERE trace_id = '{hex}'"),
    )
    .await
}

/// The single flushed spans file of the project.
async fn spans_file(storage: &crate::Storage) -> crate::catalog::FileMeta {
    let files = storage.catalog_files(Table::Spans);
    assert_eq!(files.len(), 1, "one flushed spans file");
    files[0].as_ref().clone()
}

/// The raw 16-byte trace ids stored in one spans Parquet object.
fn spans_trace_ids(dir: &Path, file: &crate::catalog::FileMeta) -> Vec<[u8; 16]> {
    let batch = object_rows(dir, file);
    trace_ids_of(&batch)
}

/// Appends one span per `(trace, span)` plus a session-target score, then
/// flushes by shutting down and reopening. Everything is ingested at `INGEST`.
const INGEST: i64 = 1_000_000_000_000;

async fn flushed_with_score(dir: &tempfile::TempDir, spans: &[(u16, u8)]) -> crate::Storage {
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    let rows: Vec<_> = spans
        .iter()
        .map(|(trace, span)| span_row(*trace, *span, INGEST, INGEST))
        .collect();
    storage
        .append_spans(project(), span_rows(&rows))
        .await
        .unwrap();
    let mut score = score_row(0x99, 0, INGEST, INGEST, INGEST, false, "session-score");
    score.target = ScoreTarget::Session {
        session_id: "session-of-another-user".to_owned(),
    };
    storage
        .append_scores(project(), score_rows(&[score]))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();
    crate::Storage::open(config_in(dir.path())).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traces_deletion_hides_memtable_rows_up_to_the_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(
            project(),
            span_rows(&[
                span_row(1, 0, INGEST, INGEST),
                span_row(2, 0, INGEST, INGEST),
            ]),
        )
        .await
        .unwrap();
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();
    assert_eq!(span_count(&storage, 1).await, 0);
    assert_eq!(span_count(&storage, 2).await, 1);

    // A row appended after the registration with a later ingest time is new
    // data, not covered by the deletion (spec 03 §7).
    storage
        .append_spans(project(), span_rows(&[span_row(1, 1, INGEST, INGEST + 1)]))
        .await
        .unwrap();
    assert_eq!(span_count(&storage, 1).await, 1);
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traces_deletion_hides_flushed_rows_and_keeps_a_session_score() {
    let dir = tempfile::tempdir().unwrap();
    let storage = flushed_with_score(&dir, &[(1, 0), (2, 0)]).await;
    // Registered after the flush: the file still holds trace 1's row, only the
    // query path hides it (the rewriter is M5.4).
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();

    let stored = spans_trace_ids(dir.path(), &spans_file(&storage).await);
    let trace_one = *span_row(1, 0, 0, 0).trace_id.as_bytes();
    assert!(stored.contains(&trace_one), "the file still holds trace 1");

    assert_eq!(span_count(&storage, 1).await, 0);
    assert_eq!(span_count(&storage, 2).await, 1);
    assert_eq!(
        scalar(&storage, "SELECT count(*) FROM spans").await,
        1,
        "only trace 2's span remains visible"
    );
    assert_eq!(
        scalar(
            &storage,
            "SELECT count(*) FROM (SELECT span_id FROM spans LIMIT 1)"
        )
        .await,
        1,
        "a LIMIT 1 query is right"
    );
    assert_eq!(trace_count(&storage, 1).await, 0);
    assert_eq!(trace_count(&storage, 2).await, 1);
    assert_eq!(
        scalar(&storage, "SELECT count(*) FROM scores").await,
        1,
        "a session-target score stays visible"
    );
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_deletion_hides_user_rows_and_session_scores_only() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    let mut mine = span_row(1, 0, INGEST, INGEST);
    mine.user_id = Some("u".to_owned());
    let no_one = span_row(2, 0, INGEST, INGEST);
    let mut shared = span_row(3, 0, INGEST, INGEST);
    shared.user_id = Some("v".to_owned());
    shared.session_id = Some("s".to_owned());
    storage
        .append_spans(project(), span_rows(&[mine, no_one, shared]))
        .await
        .unwrap();
    let mut session_score = score_row(0x11, 0, INGEST, INGEST, INGEST, false, "score");
    session_score.target = ScoreTarget::Session {
        session_id: "s".to_owned(),
    };
    storage
        .append_scores(project(), score_rows(&[session_score]))
        .await
        .unwrap();

    storage
        .register_deletion(Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: INGEST,
            scope: DeletionScope::User {
                user_id: "u".to_owned(),
                trace_ids: Vec::new(),
                session_ids: vec!["s".to_owned()],
            },
        })
        .await
        .unwrap();

    assert_eq!(
        scalar(&storage, "SELECT count(*) FROM spans WHERE user_id = 'u'").await,
        0,
        "the user's spans are hidden"
    );
    assert_eq!(
        scalar(&storage, "SELECT count(*) FROM spans WHERE user_id IS NULL").await,
        1,
        "spans with no user stay"
    );
    assert_eq!(
        scalar(
            &storage,
            "SELECT count(*) FROM spans WHERE session_id = 's'"
        )
        .await,
        1,
        "another user's span in the session stays"
    );
    assert_eq!(
        scalar(&storage, "SELECT count(*) FROM scores").await,
        0,
        "the session's scores are hidden"
    );
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn debug_does_not_show_an_erased_users_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let storage = flushed_with_score(&dir, &[(1, 0)]).await;
    storage
        .register_deletion(Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: INGEST,
            scope: DeletionScope::User {
                user_id: "erased@example.com".to_owned(),
                trace_ids: Vec::new(),
                session_ids: vec!["erased-session".to_owned()],
            },
        })
        .await
        .unwrap();

    let query = storage
        .query(project(), QueryOptions::new(None, None))
        .await
        .unwrap();
    let engine = query.engine();
    let ctx = QueryContext::new(query.clone(), None).unwrap();
    let tombstones = storage.published.load().tombstones.for_project(project());
    for debug in [
        format!("{query:?}"),
        format!("{ctx:?}"),
        format!("{:?}", engine.spans),
        format!("{:?}", engine.traces),
        format!("{:?}", engine.scores),
        format!("{tombstones:?}"),
    ] {
        assert!(
            !debug.contains("erased@example.com") && !debug.contains("erased-session"),
            "debug leaked erased identifier:\n{debug}"
        );
    }
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raised_file_escapes_the_tombstone_filter() {
    let dir = tempfile::tempdir().unwrap();
    let storage = flushed_with_score(&dir, &[(1, 0)]).await;
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();

    assert_eq!(
        scalar(&storage, "SELECT count(*) AS n FROM spans").await,
        0,
        "before the raise the gated file is filtered"
    );

    // Raising `deletions_applied_seq` without rewriting the file proves the
    // scan skips tombstone filtering once the sequence covers the deletion.
    let file = spans_file(&storage).await.id;
    storage
        .commit_test_edit(&crate::catalog::CatalogEdit {
            add: Vec::new(),
            remove: Vec::new(),
            flushed_seq: None,
            sources: Vec::new(),
            raise: vec![(file, 1)],
            quarantine: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        scalar(&storage, "SELECT count(*) AS n FROM spans").await,
        1,
        "after the raise the clean file is not filtered"
    );
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_limit_over_clean_and_gated_sides_sits_above_the_union() {
    let dir = tempfile::tempdir().unwrap();
    // Two flushes: one file holds the covered trace 2, the other the visible
    // traces 3 and 4. Nothing stays in the memtable.
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(project(), span_rows(&[span_row(2, 0, INGEST, INGEST)]))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(
            project(),
            span_rows(&[
                span_row(3, 0, INGEST, INGEST),
                span_row(4, 0, INGEST, INGEST),
            ]),
        )
        .await
        .unwrap();
    storage.shutdown().await.unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .register_deletion(traces_deletion(&[2], INGEST))
        .await
        .unwrap();

    // Raise the file with no covered row: it becomes the clean side.
    let files = storage.catalog_files(Table::Spans);
    assert_eq!(files.len(), 2);
    let covered = *span_row(2, 0, 0, 0).trace_id.as_bytes();
    let clean = files
        .iter()
        .find(|file| !spans_trace_ids(dir.path(), file).contains(&covered))
        .expect("one file holds no covered row");
    storage
        .commit_test_edit(&crate::catalog::CatalogEdit {
            add: Vec::new(),
            remove: Vec::new(),
            flushed_seq: None,
            sources: Vec::new(),
            raise: vec![(clean.id, 1)],
            quarantine: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        scalar(
            &storage,
            "SELECT count(*) FROM (SELECT span_id FROM spans LIMIT 1)"
        )
        .await,
        1,
        "the limit returns exactly one row"
    );
    assert_eq!(scalar(&storage, "SELECT count(*) FROM spans").await, 2);
    assert_eq!(
        rows(
            &context(&storage).await,
            "SELECT span_id FROM spans LIMIT 1"
        )
        .await
        .len(),
        1,
        "LIMIT 1 returns one row across the union of clean and gated files"
    );
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hidden_rows_stay_hidden_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(
            project(),
            span_rows(&[
                span_row(1, 0, INGEST, INGEST),
                span_row(2, 0, INGEST, INGEST),
            ]),
        )
        .await
        .unwrap();
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();

    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    assert_eq!(span_count(&storage, 1).await, 0);
    assert_eq!(span_count(&storage, 2).await, 1);
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_deletion_is_idempotent_and_status_reports_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    let request = traces_deletion(&[1], 1_000);
    storage.register_deletion(request.clone()).await.unwrap();
    storage.register_deletion(request.clone()).await.unwrap();
    assert_eq!(
        storage.deletion_status(request.id).await.unwrap(),
        DeletionStatus::Pending
    );
    let error = storage.deletion_status(Uuid::now_v7()).await.unwrap_err();
    assert!(matches!(error, DeletionError::NotFound), "{error}");
    let mut invalid = traces_deletion(&[1], 0);
    invalid.cutoff = 0;
    let error = storage.register_deletion(invalid).await.unwrap_err();
    assert!(matches!(error, DeletionError::Invalid { .. }), "{error}");
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_store_runs_no_pass() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(project(), span_rows(&[span_row(1, 0, INGEST, INGEST)]))
        .await
        .unwrap();
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();

    // The next catalog commit fails with a fsync error, which stops the
    // committer (D-R-6). The deletion above stays pending.
    storage.hooks().arm_fail(HookPoint::CatalogFsync);
    storage
        .register_deletion(traces_deletion(&[2], INGEST))
        .await
        .unwrap_err();
    assert!(storage.health().catalog_failed);

    // Neither pass attempts its work: each returns an empty report.
    let deletions = storage.apply_deletions(INGEST + 1).await.unwrap();
    assert_eq!(deletions, DeletionReport::default());
    let compacted = storage.compact(INGEST + 1).await.unwrap();
    assert_eq!(compacted, CompactionReport::default());
    // A status read goes through the stopped committer too, so it is refused.
    let error = storage.deletion_status(Uuid::now_v7()).await.unwrap_err();
    assert!(
        matches!(error, DeletionError::Catalog(CatalogError::Stopped)),
        "{error}"
    );
    storage.shutdown().await.unwrap();
}

/// Marks a span's name so the test can find its bytes in a WAL file.
const MARKER: &str = "erased-span-marker-4c1e9a7b";

/// Whether any file under `dir` holds `marker`.
fn files_contain(dir: &Path, marker: &[u8]) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if files_contain(&path, marker) {
                return true;
            }
        } else if std::fs::read(&path)
            .is_ok_and(|bytes| bytes.windows(marker.len()).any(|window| window == marker))
        {
            return true;
        }
    }
    false
}

/// Appends a span named [`MARKER`] on trace 1 and a plain span on trace 2, with
/// the flush parked after its freeze. Returns once the flush is parked and the
/// marker is in the WAL, so the later check cannot pass for the wrong reason.
async fn park_flush_over_marked_span(storage: &crate::Storage, dir: &Path) {
    let hooks = storage.hooks().clone();
    hooks.hold(HookPoint::FlushAfterFreeze);
    let mut marked = span_row(1, 0, INGEST, INGEST);
    marked.name = MARKER.to_owned();
    storage
        .append_spans(
            project(),
            span_rows(&[marked, span_row(2, 0, INGEST, INGEST)]),
        )
        .await
        .unwrap();
    let started = Instant::now();
    while hooks.waiting_at(HookPoint::FlushAfterFreeze) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "flush did not freeze"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        files_contain(&dir.join("wal"), MARKER.as_bytes()),
        "the marker is in the WAL before the flush"
    );
}

/// Waits until the parked flush has committed and the memtable is empty.
async fn wait_for_flush_commit(storage: &crate::Storage, rows: u64) {
    let started = Instant::now();
    while storage.catalog_rows(Table::Spans) != rows || storage.published_rows(Table::Spans) != 0 {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "flush did not commit {rows} rows"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until no file under `dir/wal` holds [`MARKER`]. The flush deletes the
/// drained segment just after its commit, so the check is polled.
async fn wait_until_wal_lacks_marker(dir: &Path) {
    let started = Instant::now();
    while files_contain(&dir.join("wal"), MARKER.as_bytes()) {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the WAL still holds the marker"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_erased_span_leaves_no_wal_bytes_once_its_deletion_applies() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_in(dir.path());
    config.flush.max_age = Duration::from_millis(100);
    let storage = crate::Storage::open(config).await.unwrap();
    park_flush_over_marked_span(&storage, dir.path()).await;

    let deletion = traces_deletion(&[1], INGEST);
    let id = deletion.id;
    storage.register_deletion(deletion).await.unwrap();
    storage.hooks().release(HookPoint::FlushAfterFreeze);
    // The flush read its tombstones before the registration, so both rows reach
    // a file; the deletion pass then rewrites the file without trace 1.
    wait_for_flush_commit(&storage, 2).await;

    let mut status = storage.deletion_status(id).await.unwrap();
    for _ in 0..50 {
        if matches!(status, DeletionStatus::Applied { .. }) {
            break;
        }
        storage
            .apply_deletions(crate::catalog::now_ns())
            .await
            .unwrap();
        status = storage.deletion_status(id).await.unwrap();
    }
    assert!(
        matches!(status, DeletionStatus::Applied { .. }),
        "deletion not applied: {status:?}"
    );
    wait_until_wal_lacks_marker(dir.path()).await;
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_project_leaves_no_wal_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_in(dir.path());
    config.flush.max_age = Duration::from_millis(100);
    let storage = crate::Storage::open(config).await.unwrap();
    park_flush_over_marked_span(&storage, dir.path()).await;

    storage.drop_project(project()).await.unwrap();
    storage.hooks().release(HookPoint::FlushAfterFreeze);
    // The flush skips the dropped project's partitions, so nothing is written.
    wait_for_flush_commit(&storage, 0).await;
    wait_until_wal_lacks_marker(dir.path()).await;
    storage.shutdown().await.unwrap();
}

/// A deletion the traces file has already applied must not hide a trace ingested after it,
/// even while another deletion of the project is pending (spec 03 §7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reingested_trace_survives_another_pending_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    storage
        .append_spans(project(), span_rows(&[span_row(1, 0, INGEST, INGEST)]))
        .await
        .unwrap();
    storage
        .register_deletion(traces_deletion(&[1], INGEST))
        .await
        .unwrap();
    storage
        .append_spans(project(), span_rows(&[span_row(1, 1, INGEST, INGEST + 10)]))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();
    let storage = crate::Storage::open(config_in(dir.path())).await.unwrap();
    assert_eq!(span_count(&storage, 1).await, 1);
    assert_eq!(trace_count(&storage, 1).await, 1);
    storage
        .register_deletion(traces_deletion(&[2], INGEST + 100))
        .await
        .unwrap();
    assert_eq!(span_count(&storage, 1).await, 1);
    assert_eq!(trace_count(&storage, 1).await, 1, "so does the trace");
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explain_redacts_tombstone_pii_and_shows_count() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config_in(dir.path());
    cfg.flush.max_age = Duration::from_millis(100);
    let storage = crate::Storage::open(cfg).await.unwrap();

    let mut span = span_row(1, 1, INGEST, INGEST);
    span.user_id = Some("erased-alice@example.com".to_owned());
    span.session_id = Some("erased-session-secret-01".to_owned());
    let trace_hex = span.trace_id.to_hex();

    let mut score = score_row(1, 1, INGEST, INGEST, INGEST, false, "quality");
    score.user_id = Some("erased-alice@example.com".to_owned());
    score.session_id = Some("erased-session-secret-01".to_owned());

    storage
        .append_spans(project(), span_rows(&[span.clone()]))
        .await
        .unwrap();
    storage
        .append_scores(project(), score_rows(&[score]))
        .await
        .unwrap();
    // Wait briefly so spans/scores flush into Parquet files with `deletions_applied_seq == 0`.
    wait_for_flush_commit(&storage, 1).await;

    storage
        .register_deletion(Deletion {
            id: Uuid::now_v7(),
            project: project(),
            cutoff: INGEST + 1_000,
            scope: DeletionScope::User {
                user_id: "erased-alice@example.com".to_owned(),
                trace_ids: vec![span.trace_id],
                session_ids: vec!["erased-session-secret-01".to_owned()],
            },
        })
        .await
        .unwrap();

    let ctx = context(&storage).await;
    for table in ["spans", "traces", "scores"] {
        let out = rows(&ctx, &format!("EXPLAIN SELECT * FROM {table}")).await;
        let plan_text: String = out
            .iter()
            .filter_map(|r| r.get("plan").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plan_text.contains("deletions=1"),
            "EXPLAIN for {table} should include redacted deletions=1: {plan_text}"
        );
        assert!(
            !plan_text.contains("erased-alice@example.com"),
            "EXPLAIN for {table} leaked erased user_id: {plan_text}"
        );
        assert!(
            !plan_text.contains("erased-session-secret-01"),
            "EXPLAIN for {table} leaked erased session_id: {plan_text}"
        );
        assert!(
            !plan_text.contains(&trace_hex),
            "EXPLAIN for {table} leaked erased trace_id: {plan_text}"
        );
    }

    storage.shutdown().await.unwrap();
}
