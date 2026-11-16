//! Deterministic crash children and the durability order.
//!
//! Every `*_child` test is ignored: it re-executes this test binary with one
//! hook armed to abort the process, and its `every_*_hook_aborts_safely`
//! parent reopens the directory and finishes the interrupted work. The
//! parents are ignored too, and `just test` runs each in a test process of
//! its own.

use crate::QueryOptions;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use arrow::array::{Array, FixedSizeBinaryArray};
use bytes::Bytes;
use bytesize::ByteSize;
use datafusion::execution::context::{SessionConfig, SessionContext};
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::span::Span;
use sha2::{Digest, Sha256};

use crate::catalog::FileTable;
use crate::config::{FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig};
use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::error::AppendError;
use crate::hooks::HookPoint;
use crate::media::MediaBlob;
use crate::wal::{Table, segment_name, spans_at};

const POINTS: &[(&str, HookPoint)] = &[
    ("FlushAfterFreeze", HookPoint::FlushAfterFreeze),
    ("FlushAfterStage", HookPoint::FlushAfterStage),
    ("ObjectWrite", HookPoint::ObjectWrite),
    ("ObjectSync", HookPoint::ObjectSync),
    ("ObjectRename", HookPoint::ObjectRename),
    ("ObjectDirSync", HookPoint::ObjectDirSync),
    ("FlushAfterWrite", HookPoint::FlushAfterWrite),
    ("CatalogCommit", HookPoint::CatalogCommit),
    ("FlushAfterCommit", HookPoint::FlushAfterCommit),
    ("FlushAfterRotate", HookPoint::FlushAfterRotate),
    ("FlushAfterTruncate", HookPoint::FlushAfterTruncate),
];

const COMPACTION_POINTS: &[(&str, HookPoint)] = &[
    ("CompactionAfterStage", HookPoint::CompactionAfterStage),
    ("ObjectSync", HookPoint::ObjectSync),
    ("ObjectRename", HookPoint::ObjectRename),
    ("ObjectDirSync", HookPoint::ObjectDirSync),
    ("CompactionAfterWrite", HookPoint::CompactionAfterWrite),
    ("CatalogCommit", HookPoint::CatalogCommit),
    ("CompactionAfterCommit", HookPoint::CompactionAfterCommit),
    ("GcObsoleteBeforeDelete", HookPoint::GcObsoleteBeforeDelete),
    ("GcObsoleteAfterDelete", HookPoint::GcObsoleteAfterDelete),
];

pub(crate) const DELETION_POINTS: &[(&str, HookPoint)] = &[
    ("CatalogCommit", HookPoint::CatalogCommit),
    ("CompactionAfterStage", HookPoint::CompactionAfterStage),
    ("ObjectSync", HookPoint::ObjectSync),
    ("ObjectRename", HookPoint::ObjectRename),
    ("CompactionAfterWrite", HookPoint::CompactionAfterWrite),
    ("CompactionAfterCommit", HookPoint::CompactionAfterCommit),
    ("GcObsoleteBeforeDelete", HookPoint::GcObsoleteBeforeDelete),
    ("GcObsoleteAfterDelete", HookPoint::GcObsoleteAfterDelete),
];

const MEDIA_POINTS: &[(&str, HookPoint)] = &[
    ("ObjectSync", HookPoint::ObjectSync),
    ("ObjectRename", HookPoint::ObjectRename),
    ("MediaAfterPut", HookPoint::MediaAfterPut),
];

const DROP_POINTS: &[(&str, HookPoint)] = &[
    ("CatalogCommit", HookPoint::CatalogCommit),
    ("MediaBeforeDelete", HookPoint::MediaBeforeDelete),
];

const RETENTION_POINTS: &[(&str, HookPoint)] = &[("CatalogCommit", HookPoint::CatalogCommit)];

/// Retention child cases: each hook in [`RETENTION_POINTS`], plus a crash after
/// `apply_retention` returns while rows of the dropped day are still only in the WAL.
const RETENTION_CASES: &[&str] = &["CatalogCommit", AFTER_RETENTION];

const AFTER_RETENTION: &str = "AfterRetention";

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

fn quiet_config(dir: &Path) -> StorageConfig {
    StorageConfig {
        delete_objects: true,
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig::default(),
        flush: FlushConfig {
            // Stay under the age and size triggers until `shutdown` flushes.
            max_bytes: ByteSize::mib(256),
            max_age: Duration::from_secs(60 * 60),
            upload_concurrency: 2,
        },
        memtable: MemtableConfig::default(),
        query: QueryConfig::default(),
        anchor: Default::default(),
        gc: Default::default(),
        compaction: Default::default(),
        deletion: Default::default(),
    }
}

fn parse_point(name: &str) -> Option<HookPoint> {
    const LISTS: &[&[(&str, HookPoint)]] = &[
        POINTS,
        COMPACTION_POINTS,
        DELETION_POINTS,
        MEDIA_POINTS,
        DROP_POINTS,
        RETENTION_POINTS,
    ];
    LISTS
        .iter()
        .flat_map(|list| list.iter())
        .find(|(label, _)| *label == name)
        .map(|(_, point)| *point)
}

/// Two batches, four rows, distinct span ids.
fn batches() -> Vec<arrow::array::RecordBatch> {
    (0..2).map(batch_rows).collect()
}

fn batch_rows(batch: u8) -> arrow::array::RecordBatch {
    let project_id = project();
    let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736"
        .parse::<TraceId>()
        .unwrap();
    let spans: Vec<Span> = (0..2_u8)
        .map(|row| {
            let mut bytes = [0_u8; 8];
            bytes[0] = batch.saturating_add(1);
            bytes[7] = row.saturating_add(1);
            let start = 1_000 + i64::from(batch) * 10 + i64::from(row);
            Span {
                project_id,
                trace_id,
                span_id: SpanId::from_bytes(bytes).unwrap(),
                parent_span_id: None,
                name: format!("b{batch}/{row}"),
                span_kind: SpanKind::Span,
                start_time: start,
                end_time: start + 100,
                status: SpanStatus::Ok,
                status_message: None,
                service_name: None,
                session_id: None,
                user_id: None,
                environment: None,
                release: None,
                tags: Vec::new(),
                metadata: BTreeMap::new(),
                provider: None,
                request_model: None,
                response_model: None,
                operation: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                total_tokens: None,
                cost_input: None,
                cost_output: None,
                cost_total: None,
                cost_source: None,
                ttft_ns: None,
                request_params: None,
                input: None,
                input_format: None,
                output: None,
                output_format: None,
                input_preview: None,
                output_preview: None,
                system_instructions: None,
                tool_definitions: None,
                attributes: BTreeMap::new(),
                resource_attributes: BTreeMap::new(),
                scope_name: None,
                scope_version: None,
                events: Vec::new(),
                links: Vec::new(),
                media_refs: Vec::new(),
                truncated: Vec::new(),
                conventions: Vec::new(),
                ingest_time: start + 200,
                schema_version: 1,
            }
        })
        .collect();
    spans_to_record_batch(&spans).unwrap()
}

fn expected_ids() -> Vec<Vec<u8>> {
    let mut ids = Vec::new();
    for batch in 0..2 {
        for row in 0..2 {
            let mut bytes = [0_u8; 8];
            bytes[0] = batch + 1;
            bytes[7] = row + 1;
            ids.push(bytes.to_vec());
        }
    }
    ids.sort();
    ids
}

/// The sorted span ids `compaction_child` appends in its step 1: the same
/// `span_row` calls, so the expectation cannot drift from what it writes.
fn compaction_span_ids() -> Vec<Vec<u8>> {
    let mut ids = Vec::new();
    for index in 0..3_u8 {
        for span in [index * 2, index * 2 + 1] {
            ids.push(
                crate::test_support::span_row(0, span, 0, 0)
                    .span_id
                    .as_bytes()
                    .to_vec(),
            );
        }
    }
    ids.sort();
    ids
}

async fn stored_ids(storage: &crate::Storage) -> Vec<Vec<u8>> {
    let query = storage
        .query(project(), QueryOptions::new(None, None))
        .await
        .unwrap();
    let ctx = SessionContext::new_with_config_rt(
        SessionConfig::new(),
        std::sync::Arc::clone(&query.runtime),
    );
    ctx.register_table("spans_t", std::sync::Arc::clone(&query.spans))
        .unwrap();
    let batches = ctx
        .sql("SELECT span_id FROM spans_t")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let mut ids = Vec::new();
    for batch in batches {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        for index in 0..batch.num_rows() {
            ids.push(column.value(index).to_vec());
        }
    }
    ids.sort();
    ids
}

fn objects_match_catalog(storage: &crate::Storage, dir: &Path) {
    for table in [Table::Spans, Table::Scores] {
        for file in storage.catalog_files(table) {
            let path = dir.join("objects").join(&file.path);
            let meta = std::fs::metadata(&path)
                .unwrap_or_else(|error| panic!("catalog file {} is missing: {error}", file.path));
            assert_eq!(meta.len(), file.size, "size of {}", file.path);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_flush_hook_aborts_safely"]
async fn child() {
    let Ok(dir) = std::env::var("LLMOBS_FLUSH_CRASH_DIR") else {
        return;
    };
    crate::test_support::no_core_dumps();
    let name = std::env::var("LLMOBS_FLUSH_CRASH_POINT").unwrap();
    let point = parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}"));
    let _lossy = lossy_from_env(Path::new(&dir));
    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    for batch in batches() {
        storage.append_spans(project(), batch).await.unwrap();
    }
    storage.hooks().abort_at(point);
    storage.shutdown().await.unwrap();
    panic!("shutdown finished without reaching {name}");
}

/// Lossy seeds per crash point: `LLMOBS_LOSSY_SEEDS`, default 1 (R2.10).
pub(crate) fn seeds() -> u64 {
    std::env::var("LLMOBS_LOSSY_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(1)
}

/// With `LLMOBS_LOSSY_SHADOW` and `LLMOBS_LOSSY_SEED` set: registers `dir` in
/// lossy mode and arms the crash before the hook abort (R2.2, R2.10). The
/// registration lives until the abort. Once `LLMOBS_LOSSY_SHADOW` is set, the
/// seed must be too: a child that lost it would abort without its crash.
fn lossy_from_env(dir: &Path) -> Option<crate::fs::lossy::Lossy> {
    let shadow = std::env::var_os("LLMOBS_LOSSY_SHADOW")?;
    let seed = std::env::var("LLMOBS_LOSSY_SEED")
        .expect("LLMOBS_LOSSY_SEED is set whenever LLMOBS_LOSSY_SHADOW is");
    let seed: u64 = seed
        .parse()
        .expect("LLMOBS_LOSSY_SEED is a u64, set by run_lossy_child");
    let lossy = crate::fs::lossy::Lossy::enable(dir, Path::new(&shadow));
    lossy.crash_on_abort(seed);
    Some(lossy)
}

/// [`run_child`] with the lossy variables of [`lossy_from_env`].
fn run_lossy_child(
    test: &str,
    dir_var: &str,
    point_var: &str,
    name: &str,
    dir: &Path,
    shadow: &Path,
    seed: u64,
) {
    let seed = seed.to_string();
    run_child(
        test,
        dir_var,
        point_var,
        name,
        dir,
        &[
            ("LLMOBS_LOSSY_SHADOW", shadow.as_os_str()),
            ("LLMOBS_LOSSY_SEED", OsStr::new(&seed)),
        ],
    );
}

/// No object on disk that the catalog does not name, after the reopen and any
/// GC step (the helper counts staged files as named).
async fn assert_no_orphans(dir: &Path, name: &str) {
    let orphans = crate::test_support::orphan_objects(dir).await;
    assert!(
        orphans.is_empty(),
        "{name}: orphan objects remain: {orphans:?}"
    );
}

/// Spawns the `test` crash child with `dir`, `name` and `extra` env vars, its stderr going to
/// `stderr`, and returns its exit status once it has ended, or fails after 60 s. The caller
/// says what an end means: [`run_child`] wants an abort by signal.
pub(crate) fn spawn_child(
    test: &str,
    dir_var: &str,
    point_var: &str,
    name: &str,
    dir: &Path,
    extra: &[(&str, &OsStr)],
    stderr: Stdio,
) -> ExitStatus {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(dir_var, dir)
        .env(point_var, name)
        .envs(extra.iter().copied())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{name} did not abort within 60s");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("waiting for {name}: {error}"),
        }
    }
}

/// [`spawn_child`] that must abort by signal. A point the child's operations
/// never reach exits with a code (its `panic!`) and fails the assertion.
fn run_child(
    test: &str,
    dir_var: &str,
    point_var: &str,
    name: &str,
    dir: &Path,
    extra: &[(&str, &OsStr)],
) {
    let status = spawn_child(test, dir_var, point_var, name, dir, extra, Stdio::inherit());
    assert!(
        status.code().is_none(),
        "{name} exited {status} instead of aborting (point not reached?)"
    );
}

/// Run alone by `just test`. A spawned child holds a copy of every open file
/// descriptor until it execs, including other tests' `LOCK` files, so a
/// concurrent test that reopens its directory in that window gets `Locked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_flush_hook_aborts_safely() {
    for (name, _) in POINTS {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::child",
            "LLMOBS_FLUSH_CRASH_DIR",
            "LLMOBS_FLUSH_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        check_flush(dir.path(), name).await;
    }
}

/// The checks after the reopen, shared by the abort parent and the lossy parent
/// (R2.10).
async fn check_flush(dir: &Path, name: &str) {
    let storage = crate::Storage::open(quiet_config(dir))
        .await
        .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
    let ids = stored_ids(&storage).await;
    assert_eq!(ids, expected_ids(), "{name}: acknowledged rows");
    objects_match_catalog(&storage, dir);
    drop(storage);
    assert_no_orphans(dir, name).await;
}

/// `quiet_config` with hand-run compaction passes: interval 1 h, two L0 files.
fn compaction_config(dir: &Path) -> StorageConfig {
    let mut config = quiet_config(dir);
    config.compaction.interval = Duration::from_secs(3_600);
    config.compaction.l0_files = 2;
    config
}

/// Day D at `hour:minute`, in ns.
fn crash_at(hour: i64, minute: i64) -> i64 {
    use chrono::Datelike;
    let day = chrono::NaiveDate::from_ymd_opt(2026, 3, 5).unwrap();
    (i64::from(day.num_days_from_ce()) - 719_163) * 86_400_000_000_000
        + (hour * 3_600 + minute * 60) * 1_000_000_000
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_compaction_hook_aborts_safely"]
async fn compaction_child() {
    let Ok(dir) = std::env::var("LLMOBS_COMPACTION_CRASH_DIR") else {
        return;
    };
    crate::test_support::no_core_dumps();
    let name = std::env::var("LLMOBS_COMPACTION_CRASH_POINT").unwrap();
    let point = parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}"));
    let _lossy = lossy_from_env(Path::new(&dir));

    // Three L0 spans files and three traces files of one trace.
    for index in 0..3_u8 {
        let storage = crate::Storage::open(compaction_config(Path::new(&dir)))
            .await
            .unwrap();
        let start = crash_at(10, 0) + i64::from(index) * 60 * 1_000_000_000;
        let spans = vec![
            crate::test_support::span_row(0, index * 2, start, start + 1_000_000_000),
            crate::test_support::span_row(
                0,
                index * 2 + 1,
                start + 1_000_000_000,
                start + 2_000_000_000,
            ),
        ];
        storage
            .append_spans(project(), crate::test_support::span_rows(&spans))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    let storage = crate::Storage::open(compaction_config(Path::new(&dir)))
        .await
        .unwrap();
    storage.hooks().abort_at(point);
    storage.compact(crash_at(12, 0)).await.unwrap();
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    panic!("compaction finished without reaching {name}");
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]: a spawned
/// child holds a copy of every open file descriptor until it execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_compaction_hook_aborts_safely() {
    for (name, _) in COMPACTION_POINTS {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::compaction_child",
            "LLMOBS_COMPACTION_CRASH_DIR",
            "LLMOBS_COMPACTION_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        check_compaction(dir.path(), name).await;
    }
}

/// The checks after the reopen, shared by the abort parent and the lossy parent
/// (R2.10).
async fn check_compaction(dir: &Path, name: &str) {
    // The reopen runs the open-time `file_sources` check and the startup
    // temp-file cleanup.
    let storage = crate::Storage::open(compaction_config(dir))
        .await
        .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
    let ids = stored_ids(&storage).await;
    assert_eq!(ids, compaction_span_ids(), "{name}: acknowledged rows");
    objects_match_catalog(&storage, dir);

    // Redo the interrupted work, then collect everything it left.
    for pass in 0..5 {
        let report = storage.compact(crash_at(12, 0)).await.unwrap();
        assert_eq!(report.failed, 0, "{name}: pass {pass} failed a job");
        if report.jobs == 0 {
            break;
        }
    }
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    storage.collect_staged(Duration::ZERO).await.unwrap();

    let ids = stored_ids(&storage).await;
    assert_eq!(ids, compaction_span_ids(), "{name}: rows after the redo");
    let spans_files = storage.catalog_files(Table::Spans);
    assert_eq!(spans_files.len(), 1, "{name}: one spans file");
    assert_eq!(spans_files[0].level, 1, "{name}: the spans file is level 1");
    let traces_files = storage.catalog_files(FileTable::Traces);
    assert_eq!(traces_files.len(), 1, "{name}: one traces file");
    assert_eq!(
        traces_files[0].level, 1,
        "{name}: the traces file is level 1"
    );
    storage.shutdown().await.unwrap();
    drop(storage);

    let (obsolete, staged) = catalog_counts(dir).await;
    assert_eq!(obsolete, 0, "{name}: obsolete rows remain");
    assert_eq!(staged, 0, "{name}: staged rows remain");
    let temps = temp_files(&dir.join("objects"));
    assert!(
        temps.is_empty(),
        "{name}: {} temp files remain",
        temps.len()
    );
    assert_no_orphans(dir, name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_deletion_hook_aborts_safely"]
async fn deletion_child() {
    let Ok(dir) = std::env::var("LLMOBS_DELETION_CRASH_DIR") else {
        return;
    };
    crate::test_support::no_core_dumps();
    let name = std::env::var("LLMOBS_DELETION_CRASH_POINT").unwrap();
    let point = parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}"));
    let _lossy = lossy_from_env(Path::new(&dir));

    // Three L0 spans files and three traces files, one span of trace 0 and
    // one of trace 1 each.
    for index in 0..3_u8 {
        let storage = crate::Storage::open(compaction_config(Path::new(&dir)))
            .await
            .unwrap();
        let start = crash_at(10, 0) + i64::from(index) * 60 * 1_000_000_000;
        let spans = vec![
            crate::test_support::span_row(0, index, start, start + 1_000_000_000),
            crate::test_support::span_row(1, index, start + 1_000_000_000, start + 2_000_000_000),
        ];
        storage
            .append_spans(project(), crate::test_support::span_rows(&spans))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    let storage = crate::Storage::open(compaction_config(Path::new(&dir)))
        .await
        .unwrap();
    storage.hooks().abort_at(point);
    storage.register_deletion(trace_deletion()).await.unwrap();
    storage.apply_deletions(crash_at(12, 0)).await.unwrap();
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    panic!("deletion pass finished without reaching {name}");
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]: a spawned
/// child holds a copy of every open file descriptor until it execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_deletion_hook_aborts_safely() {
    for (name, _) in DELETION_POINTS {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::deletion_child",
            "LLMOBS_DELETION_CRASH_DIR",
            "LLMOBS_DELETION_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        check_deletion(dir.path(), name).await;
    }
}

/// The checks after the reopen, shared by the abort parent and the lossy parent
/// (R2.10).
async fn check_deletion(dir: &Path, name: &str) {
    let storage = crate::Storage::open(compaction_config(dir))
        .await
        .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
    // New when the crash came before the registration commit, a no-op
    // after it. Either way trace 1 is hidden from both tables at once.
    storage.register_deletion(trace_deletion()).await.unwrap();
    let trace0 = trace_bytes(0);
    let trace1 = trace_bytes(1);
    let spans = stored_trace_ids(&storage, false).await;
    assert!(
        !spans.contains(&trace1),
        "{name}: a trace 1 span is visible"
    );
    assert_eq!(
        spans.iter().filter(|trace| **trace == trace0).count(),
        3,
        "{name}: trace 0 spans"
    );
    let traces = stored_trace_ids(&storage, true).await;
    assert!(
        !traces.contains(&trace1),
        "{name}: a trace 1 partial is visible"
    );
    objects_match_catalog(&storage, dir);

    // Finish the interrupted work, then collect everything it left.
    for pass in 0..5 {
        let report = storage.apply_deletions(crash_at(12, 0)).await.unwrap();
        assert_eq!(report.failed, 0, "{name}: pass {pass} failed a job");
        if report.applied == 1 || deletion_applied(&storage, name).await {
            break;
        }
    }
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    storage.collect_staged(Duration::ZERO).await.unwrap();

    // Trace 0 is intact, in the queries and in the files on disk.
    let ids = stored_ids(&storage).await;
    assert_eq!(ids, deletion_span_ids(), "{name}: acknowledged rows");
    for file in storage.catalog_files(Table::Spans) {
        let rows = crate::test_support::object_rows(dir, &file);
        let covered = crate::test_support::trace_ids_of(&rows)
            .into_iter()
            .any(|trace| trace.as_slice() == trace1.as_slice());
        assert!(!covered, "{name}: {} holds a trace 1 row", file.path);
    }
    assert!(
        deletion_applied(&storage, name).await,
        "{name}: the deletion is not applied"
    );

    let (obsolete, staged) = catalog_counts(dir).await;
    assert_eq!(obsolete, 0, "{name}: obsolete rows remain");
    assert_eq!(staged, 0, "{name}: staged rows remain");
    let temps = temp_files(&dir.join("objects"));
    assert!(
        temps.is_empty(),
        "{name}: {} temp files remain",
        temps.len()
    );
    storage.shutdown().await.unwrap();
    drop(storage);
    assert_no_orphans(dir, name).await;
}

/// Whether the child's deletion is `Applied`.
async fn deletion_applied(storage: &crate::Storage, name: &str) -> bool {
    match storage.deletion_status(deletion_id()).await.unwrap() {
        DeletionStatus::Applied { .. } => true,
        status => {
            assert!(
                matches!(status, DeletionStatus::Pending),
                "{name}: the deletion status is {status:?}"
            );
            false
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_media_hook_aborts_safely"]
async fn media_child() {
    let Ok(dir) = std::env::var("LLMOBS_MEDIA_CRASH_DIR") else {
        return;
    };
    crate::test_support::no_core_dumps();
    let name = std::env::var("LLMOBS_MEDIA_CRASH_POINT").unwrap();
    let point = parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}"));
    let _lossy = lossy_from_env(Path::new(&dir));
    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    storage.hooks().abort_at(point);
    storage.put_media(project(), media_blob()).await.unwrap();
    panic!("media put finished without reaching {name}");
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]: a spawned
/// child holds a copy of every open file descriptor until it execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_media_hook_aborts_safely() {
    for (name, _) in MEDIA_POINTS {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::media_child",
            "LLMOBS_MEDIA_CRASH_DIR",
            "LLMOBS_MEDIA_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        check_media(dir.path(), name).await;
    }
}

/// The checks after the reopen, shared by the abort parent and the lossy parent
/// (R2.10).
async fn check_media(dir: &Path, name: &str) {
    // The reopen runs the startup temp-file cleanup, under `media/` too.
    let storage = crate::Storage::open(quiet_config(dir))
        .await
        .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
    let temps = temp_files(&dir.join("objects"));
    assert!(
        temps.is_empty(),
        "{name}: {} temp files remain",
        temps.len()
    );
    storage.collect_staged(Duration::ZERO).await.unwrap();
    let (rows, objects) = media_state(dir).await;
    assert_eq!(rows, objects, "{name}: media rows and objects disagree");

    storage.put_media(project(), media_blob()).await.unwrap();
    let (rows, objects) = media_state(dir).await;
    assert_eq!(
        rows, objects,
        "{name}: media rows and objects disagree after the put"
    );
    let read = storage
        .get_media(project(), &media_blob().sha256, None, None)
        .await
        .unwrap();
    assert_eq!(read.bytes, media_blob().bytes, "{name}: the blob's bytes");
    assert_eq!(
        read.mime_type,
        media_blob().mime_type,
        "{name}: the blob's mime type"
    );
    assert_eq!(
        read.size,
        media_blob().bytes.len() as u64,
        "{name}: the blob's size"
    );
    let (_, staged) = catalog_counts(dir).await;
    assert_eq!(staged, 0, "{name}: staged rows remain");
    storage.shutdown().await.unwrap();
    drop(storage);
    assert_no_orphans(dir, name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_drop_hook_aborts_safely"]
async fn drop_child() {
    let Ok(dir) = std::env::var("LLMOBS_DROP_CRASH_DIR") else {
        return;
    };
    crate::test_support::no_core_dumps();
    let name = std::env::var("LLMOBS_DROP_CRASH_POINT").unwrap();
    let point = parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}"));
    let _lossy = lossy_from_env(Path::new(&dir));

    // One flush cycle with spans of P, and one blob of P.
    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    let start = crash_at(10, 0);
    let spans = vec![crate::test_support::span_row(
        0,
        0,
        start,
        start + 1_000_000_000,
    )];
    storage
        .append_spans(project(), crate::test_support::span_rows(&spans))
        .await
        .unwrap();
    storage.put_media(project(), media_blob()).await.unwrap();
    storage.shutdown().await.unwrap();

    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    storage.hooks().abort_at(point);
    storage.drop_project(project()).await.unwrap();
    panic!("project drop finished without reaching {name}");
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]: a spawned
/// child holds a copy of every open file descriptor until it execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_drop_hook_aborts_safely() {
    for (name, _) in DROP_POINTS {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::drop_child",
            "LLMOBS_DROP_CRASH_DIR",
            "LLMOBS_DROP_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        check_drop(dir.path(), name).await;
    }
}

/// The checks after the reopen, shared by the abort parent and the lossy parent
/// (R2.10).
async fn check_drop(dir: &Path, name: &str) {
    // The reopen finishes a crash after the drop commit; the second
    // `drop_project` is idempotent either way.
    let storage = crate::Storage::open(quiet_config(dir))
        .await
        .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
    storage.drop_project(project()).await.unwrap();
    assert!(
        stored_ids(&storage).await.is_empty(),
        "{name}: P's spans are visible"
    );
    assert!(
        stored_trace_ids(&storage, true).await.is_empty(),
        "{name}: P's partials are visible"
    );
    let error = storage
        .append_spans(
            project(),
            crate::test_support::span_rows(&[crate::test_support::span_row(0, 40, 0, 0)]),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, AppendError::ProjectDropped),
        "{name}: P's appends are not refused"
    );
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    let leftovers = project_objects(dir, project());
    assert!(
        leftovers.is_empty(),
        "{name}: {} objects of P remain",
        leftovers.len()
    );
    let temps = temp_files(&dir.join("objects"));
    assert!(
        temps.is_empty(),
        "{name}: {} temp files remain",
        temps.len()
    );
    assert_eq!(
        project_state(dir, project()).await,
        "dropped",
        "{name}: P is not dropped"
    );
    let (obsolete, staged) = catalog_counts(dir).await;
    assert_eq!(obsolete, 0, "{name}: obsolete rows remain");
    assert_eq!(staged, 0, "{name}: staged rows remain");
    storage.shutdown().await.unwrap();
    drop(storage);
    assert_no_orphans(dir, name).await;
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]. The child
/// crashes its directory with a seeded lossy crash before it aborts (R2.10):
/// `LLMOBS_LOSSY_SEEDS` seeds per point, default 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_flush_hook_survives_a_lossy_crash() {
    for (index, (name, _)) in POINTS.iter().enumerate() {
        for count in 0..seeds() {
            // Consecutive points get different seeds, so they cycle through the modes.
            let seed = index as u64 * 1_000 + count;
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("data");
            let shadow = t.path().join("shadow");
            run_lossy_child(
                "crash_tests::child",
                "LLMOBS_FLUSH_CRASH_DIR",
                "LLMOBS_FLUSH_CRASH_POINT",
                name,
                &dir,
                &shadow,
                seed,
            );
            check_flush(&dir, &format!("{name} with lossy seed {seed}")).await;
        }
    }
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]. The child
/// crashes its directory with a seeded lossy crash before it aborts (R2.10):
/// `LLMOBS_LOSSY_SEEDS` seeds per point, default 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_compaction_hook_survives_a_lossy_crash() {
    for (index, (name, _)) in COMPACTION_POINTS.iter().enumerate() {
        for count in 0..seeds() {
            // Consecutive points get different seeds, so they cycle through the modes.
            let seed = index as u64 * 1_000 + count;
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("data");
            let shadow = t.path().join("shadow");
            run_lossy_child(
                "crash_tests::compaction_child",
                "LLMOBS_COMPACTION_CRASH_DIR",
                "LLMOBS_COMPACTION_CRASH_POINT",
                name,
                &dir,
                &shadow,
                seed,
            );
            check_compaction(&dir, &format!("{name} with lossy seed {seed}")).await;
        }
    }
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]. The child
/// crashes its directory with a seeded lossy crash before it aborts (R2.10):
/// `LLMOBS_LOSSY_SEEDS` seeds per point, default 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_deletion_hook_survives_a_lossy_crash() {
    for (index, (name, _)) in DELETION_POINTS.iter().enumerate() {
        for count in 0..seeds() {
            // Consecutive points get different seeds, so they cycle through the modes.
            let seed = index as u64 * 1_000 + count;
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("data");
            let shadow = t.path().join("shadow");
            run_lossy_child(
                "crash_tests::deletion_child",
                "LLMOBS_DELETION_CRASH_DIR",
                "LLMOBS_DELETION_CRASH_POINT",
                name,
                &dir,
                &shadow,
                seed,
            );
            check_deletion(&dir, &format!("{name} with lossy seed {seed}")).await;
        }
    }
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]. The child
/// crashes its directory with a seeded lossy crash before it aborts (R2.10):
/// `LLMOBS_LOSSY_SEEDS` seeds per point, default 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_media_hook_survives_a_lossy_crash() {
    for (index, (name, _)) in MEDIA_POINTS.iter().enumerate() {
        for count in 0..seeds() {
            // Consecutive points get different seeds, so they cycle through the modes.
            let seed = index as u64 * 1_000 + count;
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("data");
            let shadow = t.path().join("shadow");
            run_lossy_child(
                "crash_tests::media_child",
                "LLMOBS_MEDIA_CRASH_DIR",
                "LLMOBS_MEDIA_CRASH_POINT",
                name,
                &dir,
                &shadow,
                seed,
            );
            check_media(&dir, &format!("{name} with lossy seed {seed}")).await;
        }
    }
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]. The child
/// crashes its directory with a seeded lossy crash before it aborts (R2.10):
/// `LLMOBS_LOSSY_SEEDS` seeds per point, default 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_drop_hook_survives_a_lossy_crash() {
    for (index, (name, _)) in DROP_POINTS.iter().enumerate() {
        for count in 0..seeds() {
            // Consecutive points get different seeds, so they cycle through the modes.
            // `1_000 % 3 == 1`, so the extra `index` moves the second point of the
            // two-point table to `TearLast` (`seed % 3 == 2`) for the default one seed.
            let seed = index as u64 * 1_000 + count + index as u64;
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join("data");
            let shadow = t.path().join("shadow");
            run_lossy_child(
                "crash_tests::drop_child",
                "LLMOBS_DROP_CRASH_DIR",
                "LLMOBS_DROP_CRASH_POINT",
                name,
                &dir,
                &shadow,
                seed,
            );
            check_drop(&dir, &format!("{name} with lossy seed {seed}")).await;
        }
    }
}

/// Run alone by `just lazyfs` (R2.12), never by `just test`. Every child of the five groups
/// runs on a LazyFS mount, and after each child exits LazyFS drops its unsynced writes, so the
/// checks see what a power loss on the real binary leaves. Needs `LLMOBS_LAZYFS_DIR` (a
/// directory on the mount), `LLMOBS_LAZYFS_FIFO` and `LLMOBS_LAZYFS_DONE`; without them it
/// returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a LazyFS mount: `just lazyfs`"]
async fn every_hook_survives_lazyfs() {
    let (Some(root), Some(fifo), Some(done)) = (
        std::env::var_os("LLMOBS_LAZYFS_DIR"),
        std::env::var_os("LLMOBS_LAZYFS_FIFO"),
        std::env::var_os("LLMOBS_LAZYFS_DONE"),
    ) else {
        return;
    };
    let (fifo, done) = (Path::new(&fifo), Path::new(&done));
    for (name, _) in POINTS {
        let dir = tempfile::tempdir_in(&root).unwrap();
        run_child(
            "crash_tests::child",
            "LLMOBS_FLUSH_CRASH_DIR",
            "LLMOBS_FLUSH_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        lazyfs_clear_cache(fifo, done);
        check_flush(dir.path(), name).await;
    }
    for (name, _) in COMPACTION_POINTS {
        let dir = tempfile::tempdir_in(&root).unwrap();
        run_child(
            "crash_tests::compaction_child",
            "LLMOBS_COMPACTION_CRASH_DIR",
            "LLMOBS_COMPACTION_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        lazyfs_clear_cache(fifo, done);
        check_compaction(dir.path(), name).await;
    }
    for (name, _) in DELETION_POINTS {
        let dir = tempfile::tempdir_in(&root).unwrap();
        run_child(
            "crash_tests::deletion_child",
            "LLMOBS_DELETION_CRASH_DIR",
            "LLMOBS_DELETION_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        lazyfs_clear_cache(fifo, done);
        check_deletion(dir.path(), name).await;
    }
    for (name, _) in MEDIA_POINTS {
        let dir = tempfile::tempdir_in(&root).unwrap();
        run_child(
            "crash_tests::media_child",
            "LLMOBS_MEDIA_CRASH_DIR",
            "LLMOBS_MEDIA_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        lazyfs_clear_cache(fifo, done);
        check_media(dir.path(), name).await;
    }
    for (name, _) in DROP_POINTS {
        let dir = tempfile::tempdir_in(&root).unwrap();
        run_child(
            "crash_tests::drop_child",
            "LLMOBS_DROP_CRASH_DIR",
            "LLMOBS_DROP_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );
        lazyfs_clear_cache(fifo, done);
        check_drop(dir.path(), name).await;
    }
}

/// LazyFS's power loss (R2.12): writes `lazyfs::clear-cache` to `fifo` and waits, at most 30 s,
/// for one line on `done`. The copy in `tests/crash.rs` does the same; integration tests cannot
/// reach this module.
fn lazyfs_clear_cache(fifo: &Path, done: &Path) {
    let (fifo, done) = (fifo.to_owned(), done.to_owned());
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(send_clear_cache(&fifo, &done));
    });
    match received.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("lazyfs clear-cache failed: {error}"),
        Err(_) => panic!("lazyfs did not acknowledge clear-cache within 30s"),
    }
}

fn send_clear_cache(fifo: &Path, done: &Path) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    std::fs::OpenOptions::new()
        .write(true)
        .open(fifo)?
        .write_all(b"lazyfs::clear-cache\n")?;
    let mut line = String::new();
    if BufReader::new(std::fs::File::open(done)?).read_line(&mut line)? == 0 {
        return Err(std::io::Error::other("the done FIFO closed without a line"));
    }
    Ok(())
}

/// `obsolete` and `staged_objects` row counts of `catalog.db`.
async fn catalog_counts(dir: &Path) -> (i64, i64) {
    let mut connection = connect_readonly(dir).await;
    let obsolete = sqlx::query_scalar("SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let staged = sqlx::query_scalar("SELECT COUNT(*) FROM staged_objects")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    (obsolete, staged)
}

/// The `trace_id` bytes of every row of the project's `spans` (`traces` is
/// false) or `traces` provider, sorted.
async fn stored_trace_ids(storage: &crate::Storage, traces: bool) -> Vec<Vec<u8>> {
    let query = storage
        .query(project(), QueryOptions::new(None, None))
        .await
        .unwrap();
    let ctx = SessionContext::new_with_config_rt(
        SessionConfig::new(),
        std::sync::Arc::clone(&query.runtime),
    );
    let provider = if traces { query.traces } else { query.spans };
    ctx.register_table("t", provider).unwrap();
    let batches = ctx
        .sql("SELECT trace_id FROM t")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let mut ids = Vec::new();
    for batch in batches {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        for index in 0..batch.num_rows() {
            ids.push(column.value(index).to_vec());
        }
    }
    ids.sort();
    ids
}

/// The `trace_id` bytes `span_row` builds for `trace`.
fn trace_bytes(trace: u16) -> Vec<u8> {
    crate::test_support::span_row(trace, 0, 0, 0)
        .trace_id
        .as_bytes()
        .to_vec()
}

/// The deletion id `deletion_child` and its parent share: the parent's
/// re-registration is a no-op exactly when the child's crash came after the
/// registration committed.
fn deletion_id() -> uuid::Uuid {
    uuid::Uuid::from_u128(0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00d1)
}

/// One `Traces` deletion of trace 1, cut off at 12:00 of day D.
fn trace_deletion() -> Deletion {
    Deletion {
        id: deletion_id(),
        project: project(),
        cutoff: crash_at(12, 0),
        scope: DeletionScope::Traces {
            trace_ids: vec![crate::test_support::span_row(1, 0, 0, 0).trace_id],
        },
    }
}

/// The sorted span ids `deletion_child` appends for trace 0: one per cycle.
fn deletion_span_ids() -> Vec<Vec<u8>> {
    let mut ids: Vec<Vec<u8>> = (0..3_u8)
        .map(|index| {
            crate::test_support::span_row(0, index, 0, 0)
                .span_id
                .as_bytes()
                .to_vec()
        })
        .collect();
    ids.sort();
    ids
}

/// The blob `media_child` stores.
fn media_blob() -> MediaBlob {
    let bytes = Bytes::from_static(b"crash-child-media-blob");
    MediaBlob {
        sha256: Sha256::digest(bytes.as_ref()).into(),
        mime_type: "image/png".to_owned(),
        bytes,
        newest_span_start: 0,
    }
}

/// Every file under `dir`, recursively. A missing `dir` is empty.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Every file under `objects/tables/*/project=<project>` and
/// `objects/media/<project>`: all of `project`'s objects, whatever their state
/// in the catalog.
fn project_objects(dir: &Path, project: ProjectId) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for table in [FileTable::Spans, FileTable::Scores, FileTable::Traces] {
        found.extend(files_under(
            &dir.join("objects")
                .join("tables")
                .join(table.as_str())
                .join(format!("project={project}")),
        ));
    }
    found.extend(files_under(
        &dir.join("objects").join("media").join(project.to_string()),
    ));
    found
}

/// The `media` rows' object paths and the files under `objects/media/`, both
/// sorted: equal exactly when no object is without a row and no row without
/// an object.
async fn media_state(dir: &Path) -> (Vec<String>, Vec<String>) {
    let rows: Vec<String> = sqlx_strings(
        dir,
        "SELECT 'media/' || project_id || '/' || sha256 FROM media",
    )
    .await;
    let objects_root = dir.join("objects");
    let mut objects: Vec<String> = files_under(&objects_root.join("media"))
        .into_iter()
        .map(|path| {
            path.strip_prefix(&objects_root)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    objects.sort();
    (rows, objects)
}

/// Every `String` of `sql`, sorted, over a read-only `catalog.db`.
async fn sqlx_strings(dir: &Path, sql: &'static str) -> Vec<String> {
    let mut connection = connect_readonly(dir).await;
    let mut rows: Vec<String> = sqlx::query_scalar(sql)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    rows.sort();
    rows
}

/// The `projects.state` of `project`.
async fn project_state(dir: &Path, project: ProjectId) -> String {
    let mut connection = connect_readonly(dir).await;
    sqlx::query_scalar("SELECT state FROM projects WHERE project_id = ?")
        .bind(project.to_string())
        .fetch_one(&mut connection)
        .await
        .unwrap()
}

/// A read-only connection to `catalog.db`.
async fn connect_readonly(dir: &Path) -> sqlx::SqliteConnection {
    use sqlx::ConnectOptions;
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("catalog.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap()
}

/// Every file under `dir` whose name marks a leftover temp file.
fn temp_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(temp_files(&path));
        } else {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') && name.contains(".tmp-") {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flush_syncs_the_file_before_the_catalog_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = quiet_config(dir.path());
    // Serial uploads make the two objects' hook order deterministic.
    config.flush.upload_concurrency = 1;
    let storage = crate::Storage::open(config).await.unwrap();
    storage
        .append_spans(project(), spans_at(&[1]))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();
    let steps: Vec<HookPoint> = storage
        .hooks()
        .points()
        .into_iter()
        .filter(|point| {
            matches!(
                point,
                HookPoint::ObjectWrite
                    | HookPoint::ObjectSync
                    | HookPoint::ObjectRename
                    | HookPoint::ObjectDirSync
                    | HookPoint::CatalogCommit
                    | HookPoint::FlushAfterTruncate
            )
        })
        .collect();
    assert_eq!(
        steps,
        vec![
            // The spans file, then the traces file derived from it.
            HookPoint::ObjectWrite,
            HookPoint::ObjectSync,
            HookPoint::ObjectRename,
            HookPoint::ObjectDirSync,
            HookPoint::ObjectWrite,
            HookPoint::ObjectSync,
            HookPoint::ObjectRename,
            HookPoint::ObjectDirSync,
            HookPoint::CatalogCommit,
            HookPoint::FlushAfterTruncate,
        ],
        "durability order: {steps:?}"
    );
}

/// The day retention drops: D-2, where D is `crash_at`'s day.
fn old_day() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 3, 3).unwrap()
}

/// The watermark the retention child commits: D-1, so the whole of
/// `old_day` is before it.
fn keep_from() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap()
}

/// Two spans of `old_day`, noon on that day plus `index` minutes.
fn old_day_rows(index: u8) -> arrow::array::RecordBatch {
    use chrono::Datelike;
    let midnight = (i64::from(old_day().num_days_from_ce()) - 719_163) * 86_400_000_000_000;
    let start = midnight + 12 * 3_600 * 1_000_000_000 + i64::from(index) * 60 * 1_000_000_000;
    let spans = vec![
        crate::test_support::span_row(0, index * 2, start, start + 1_000_000_000),
        crate::test_support::span_row(
            0,
            index * 2 + 1,
            start + 1_000_000_000,
            start + 2_000_000_000,
        ),
    ];
    crate::test_support::span_rows(&spans)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by every_retention_hook_aborts_safely"]
async fn retention_child() {
    let Ok(dir) = std::env::var("LLMOBS_RETENTION_CRASH_DIR") else {
        return;
    };
    let name = std::env::var("LLMOBS_RETENTION_CRASH_POINT").unwrap();
    let point = if name == AFTER_RETENTION {
        None
    } else {
        Some(parse_point(&name).unwrap_or_else(|| panic!("unknown crash point {name}")))
    };

    // A flushed batch of D-2, then a batch of D-2 that only the WAL holds.
    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    storage
        .append_spans(project(), old_day_rows(0))
        .await
        .unwrap();
    storage.shutdown().await.unwrap();

    let storage = crate::Storage::open(quiet_config(Path::new(&dir)))
        .await
        .unwrap();
    storage
        .append_spans(project(), old_day_rows(1))
        .await
        .unwrap();
    if let Some(point) = point {
        storage.hooks().abort_at(point);
    }
    storage
        .apply_retention(project(), keep_from())
        .await
        .unwrap();
    if point.is_none() {
        // The watermark is committed and the second batch was never flushed.
        std::process::abort();
    }
    panic!("retention finished without reaching {name}");
}

/// The paths of the active files of `old_day` in all three tables.
fn active_old_day_paths(storage: &crate::Storage) -> Vec<String> {
    let mut files = storage.catalog_files(Table::Spans);
    files.extend(storage.catalog_files(FileTable::Traces));
    files.extend(storage.catalog_files(Table::Scores));
    files
        .iter()
        .filter(|file| file.day == old_day())
        .map(|file| file.path.clone())
        .collect()
}

/// The `keep_from` stored for the test project, if a retention committed.
async fn stored_watermark(dir: &Path) -> Option<String> {
    let mut connection = connect_readonly(dir).await;
    sqlx::query_scalar("SELECT keep_from FROM retention_watermarks WHERE project_id = ?")
        .bind(project().to_string())
        .fetch_optional(&mut connection)
        .await
        .unwrap()
}

/// Run alone by `just test`, like [`every_flush_hook_aborts_safely`]: a spawned
/// child holds a copy of every open file descriptor until it execs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
async fn every_retention_hook_aborts_safely() {
    for name in RETENTION_CASES {
        let dir = tempfile::tempdir().unwrap();
        run_child(
            "crash_tests::retention_child",
            "LLMOBS_RETENTION_CRASH_DIR",
            "LLMOBS_RETENTION_CRASH_POINT",
            name,
            dir.path(),
            &[],
        );

        let storage = crate::Storage::open(quiet_config(dir.path()))
            .await
            .unwrap_or_else(|error| panic!("{name}: reopen failed: {error}"));
        // Either the commit never happened and the flushed D-2 files are still
        // active, or it did and no D-2 file is active.
        let watermark = stored_watermark(dir.path()).await;
        let active = active_old_day_paths(&storage);
        match &watermark {
            None => assert_eq!(active.len(), 2, "{name}: active D-2 files {active:?}"),
            Some(keep_from_stored) => {
                assert_eq!(keep_from_stored, "2026-03-04", "{name}: the watermark");
                assert!(
                    active.is_empty(),
                    "{name}: D-2 files are active: {active:?}"
                );
            }
        }
        // The flush writes the WAL-only rows. With a watermark they belong to a
        // dropped day and must not come back as a file.
        storage.shutdown().await.unwrap();
        if watermark.is_some() {
            let storage = crate::Storage::open(quiet_config(dir.path()))
                .await
                .unwrap_or_else(|error| panic!("{name}: second reopen failed: {error}"));
            let active = active_old_day_paths(&storage);
            assert!(
                active.is_empty(),
                "{name}: a D-2 file appeared after the flush: {active:?}"
            );
            storage.shutdown().await.unwrap();
        }
    }
}

// Compaction's durability order: the spans output is published, then the
// traces output is written, and only then is the catalog committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_syncs_each_output_before_the_catalog_commit() {
    let dir = tempfile::tempdir().unwrap();
    let config = compaction_config(dir.path());
    // Two L0 spans files of one day, each with its traces file.
    for index in 0..2_u8 {
        let storage = crate::Storage::open(config.clone()).await.unwrap();
        let start = crash_at(10, 0) + i64::from(index) * 60 * 1_000_000_000;
        let spans = vec![
            crate::test_support::span_row(0, index * 2, start, start + 1_000_000_000),
            crate::test_support::span_row(
                0,
                index * 2 + 1,
                start + 1_000_000_000,
                start + 2_000_000_000,
            ),
        ];
        storage
            .append_spans(project(), crate::test_support::span_rows(&spans))
            .await
            .unwrap();
        storage.shutdown().await.unwrap();
    }

    // A fresh open has an empty hook log, so only the compaction's points follow.
    let storage = crate::Storage::open(config).await.unwrap();
    let report = storage.compact(crash_at(12, 0)).await.unwrap();
    assert_eq!((report.jobs, report.failed), (1, 0), "{report:?}");
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    let steps: Vec<HookPoint> = storage
        .hooks()
        .points()
        .into_iter()
        .filter(|point| {
            matches!(
                point,
                HookPoint::ObjectWrite
                    | HookPoint::ObjectSync
                    | HookPoint::ObjectRename
                    | HookPoint::ObjectDirSync
                    | HookPoint::CompactionAfterWrite
                    | HookPoint::CatalogCommit
                    | HookPoint::CompactionAfterCommit
                    | HookPoint::GcObsoleteBeforeDelete
                    | HookPoint::GcObsoleteAfterDelete
            )
        })
        .collect();
    let commit = steps
        .iter()
        .position(|point| *point == HookPoint::CatalogCommit)
        .unwrap_or_else(|| panic!("no CatalogCommit: {steps:?}"));
    // The spans output (publish: sync, rename, dir sync), then the traces
    // output (write, sync, rename, dir sync), then the write barrier. Only
    // then the commit; no GC delete comes before it.
    let before_commit = vec![
        HookPoint::ObjectSync,
        HookPoint::ObjectRename,
        HookPoint::ObjectDirSync,
        HookPoint::ObjectWrite,
        HookPoint::ObjectSync,
        HookPoint::ObjectRename,
        HookPoint::ObjectDirSync,
        HookPoint::CompactionAfterWrite,
    ];
    assert_eq!(&steps[..commit], before_commit.as_slice(), "{steps:?}");
    assert_eq!(
        steps.get(commit + 1),
        Some(&HookPoint::CompactionAfterCommit),
        "{steps:?}"
    );
    let after = &steps[commit + 2..];
    assert!(!after.is_empty(), "collect_obsolete deleted nothing");
    assert!(
        after.iter().all(|point| matches!(
            point,
            HookPoint::GcObsoleteBeforeDelete | HookPoint::GcObsoleteAfterDelete
        )),
        "{steps:?}"
    );
}

/// Run by [`a_rotation_crash_reopens_with_the_acknowledged_rows`]. The four rows
/// are one append, so one record, and the writer keeps running. The age trigger
/// then flushes about 200 ms later and rotates; the abort lands between the new
/// segment's creation and the drained one's deletion. A single append leaves no
/// window in which a flush could fall between two appends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by a_rotation_crash_reopens_with_the_acknowledged_rows"]
async fn rotation_child() {
    let Ok(dir) = std::env::var("LLMOBS_ROTATE_CRASH_DIR") else {
        return;
    };
    let mut config = quiet_config(Path::new(&dir));
    config.flush.max_age = Duration::from_millis(200);
    let storage = crate::Storage::open(config).await.unwrap();
    storage.hooks().abort_at(HookPoint::FlushAfterRotate);
    let batches = batches();
    let one = arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap();
    storage.append_spans(project(), one).await.unwrap();
    // The age trigger fires about 200 ms after the append, well before this
    // sleep ends. The abort then ends the process.
    tokio::time::sleep(Duration::from_secs(10)).await;
    panic!("the flush did not reach FlushAfterRotate");
}

/// The rotation's crash window: the new segment exists and the drained one is
/// not yet deleted. Reopen keeps every acknowledged row once and continues the
/// sequence after the flushed records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns a process; `just test` runs it in its own test process"]
async fn a_rotation_crash_reopens_with_the_acknowledged_rows() {
    let dir = tempfile::tempdir().unwrap();
    run_child(
        "crash_tests::rotation_child",
        "LLMOBS_ROTATE_CRASH_DIR",
        "LLMOBS_ROTATE_CRASH_POINT",
        "FlushAfterRotate",
        dir.path(),
        &[],
    );
    let wal = dir.path().join("wal").join("spans");
    let mut names: Vec<String> = std::fs::read_dir(&wal)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    // Record 1 sits in the drained segment; the new one is named for seq 2.
    assert_eq!(names, vec![segment_name(1), segment_name(2)]);

    let storage = crate::Storage::open(quiet_config(dir.path()))
        .await
        .unwrap();
    assert_eq!(
        stored_ids(&storage).await,
        expected_ids(),
        "acknowledged rows"
    );
    objects_match_catalog(&storage, dir.path());
    let receipt = storage
        .append_spans(project(), spans_at(&[9_000]))
        .await
        .unwrap();
    assert_eq!(
        receipt.seq, 2,
        "the sequence continues after the flushed records"
    );
    storage.shutdown().await.unwrap();
}
