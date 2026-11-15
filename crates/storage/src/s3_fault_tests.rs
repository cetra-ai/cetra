//! Focused scenarios under injected S3 faults that the model test reaches too
//! rarely to rely on (R4.6, spec 03 §10). Each runs `Storage` over a
//! [`ChaosStore`] standing in for S3, with the read cache on.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use bytesize::ByteSize;

use object_store::ObjectStoreExt;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use sha2::{Digest, Sha256};
use sqlx::ConnectOptions;

use crate::Storage;
use crate::catalog::{FileTable, now_ns};
use crate::compaction::job::tests::{at, config_in, project, quiet_compaction};
use crate::config::StorageConfig;
use crate::error::{AnchorError, OpenError};
use crate::media::MediaBlob;
use crate::objects::{Objects, PutSizes};
use crate::read_cache::cached_name;
use crate::test_support::{
    Call, ChaosStore, Fault, Status, collect_orphans_dry_run, span_row, span_rows,
};
use crate::wal::{Table, spans_at};

/// The config of every test here: a flush every 100 ms, a 64 MiB read cache,
/// and compaction only when a test asks for it.
fn config(dir: &Path) -> StorageConfig {
    let mut config = config_in(dir, quiet_compaction(2), Duration::from_millis(100));
    config.query.read_cache_bytes = Some(ByteSize::mib(64));
    config
}

/// Small S3 sizes, so a test can get a multipart upload from a few kilobytes.
fn put_sizes() -> PutSizes {
    PutSizes {
        single_max: 16 * 1024,
        part: 8 * 1024,
    }
}

async fn open(config: StorageConfig, chaos: &ChaosStore) -> Result<Storage, OpenError> {
    Storage::open_with_store(config, Arc::new(chaos.clone()), put_sizes()).await
}

/// Polls until `rows` rows of `table` are committed and none are waiting in
/// the memtable.
async fn wait_for_catalog(storage: &Storage, table: Table, rows: u64) {
    let started = Instant::now();
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

/// Rows of the `spans` view, counted through the query path (and so the read
/// cache).
async fn span_count(storage: &Storage) -> Result<usize, datafusion::error::DataFusionError> {
    use datafusion::execution::context::SessionContext;
    use datafusion::prelude::SessionConfig;
    let query = storage.query(project(), Default::default()).await.unwrap();
    let ctx = SessionContext::new_with_config_rt(SessionConfig::new(), Arc::clone(&query.runtime));
    ctx.register_table("spans_t", query.spans)?;
    let batches = ctx.sql("SELECT name FROM spans_t").await?.collect().await?;
    Ok(batches.iter().map(|batch| batch.num_rows()).sum())
}

/// A read-only handle on `catalog.db`.
async fn catalog_db(data_dir: &Path) -> sqlx::SqliteConnection {
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(data_dir.join("catalog.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap()
}

/// The object paths of the obsolete catalog rows.
async fn obsolete_paths(data_dir: &Path) -> Vec<String> {
    let mut connection = catalog_db(data_dir).await;
    sqlx::query_scalar("SELECT path FROM data_files WHERE state = 'obsolete' ORDER BY path")
        .fetch_all(&mut connection)
        .await
        .unwrap()
}

/// Whether `path` is an object of `store`.
async fn exists(store: &Arc<InMemory>, path: &str) -> bool {
    store.head(&ObjectPath::from(path)).await.is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_fails_on_a_store_error_and_a_later_open_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    let storage = open(config(dir.path()), &chaos).await.unwrap();
    storage
        .append_spans(project(), spans_at(&[1]))
        .await
        .unwrap();
    wait_for_catalog(&storage, Table::Spans, 1).await;
    storage.shutdown().await.unwrap();

    // The flush wrote the files through to the read cache, which survives the
    // restart. Without this, the anchor rebuild would read from the cache and
    // never reach the store.
    std::fs::remove_dir_all(dir.path().join("cache")).unwrap();
    chaos.hold(&[Call::Get, Call::Head]);
    let error = match open(config(dir.path()), &chaos).await {
        Ok(_) => panic!("the open succeeded under store errors"),
        Err(error) => error,
    };
    assert!(
        chaos.count(Call::Get) + chaos.count(Call::Head) > 0,
        "the open did not reach the store"
    );
    assert!(
        matches!(error, OpenError::Anchor(AnchorError::Parquet(_))),
        "{error:?}"
    );

    chaos.release();
    let storage = open(config(dir.path()), &chaos).await.unwrap();
    assert_eq!(span_count(&storage).await.unwrap(), 1);
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verification_keeps_a_file_the_store_fails_to_read() {
    #[derive(Debug, Clone, Copy)]
    enum Case {
        HeadFails,
        GetFails,
        BodyTruncated,
    }
    for case in [Case::HeadFails, Case::GetFails, Case::BodyTruncated] {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(InMemory::new());
        let chaos = ChaosStore::new(Arc::clone(&inner) as _);
        let storage = open(config(dir.path()), &chaos).await.unwrap();
        storage
            .append_spans(project(), spans_at(&[1]))
            .await
            .unwrap();
        wait_for_catalog(&storage, Table::Spans, 1).await;
        let file = storage.catalog_files(FileTable::Spans)[0].clone();

        match case {
            Case::HeadFails => chaos.hold(&[Call::Head]),
            Case::GetFails => chaos.hold(&[Call::Get]),
            Case::BodyTruncated => {
                for _ in 0..64 {
                    chaos.push(Call::Get, Fault::Truncated);
                }
            }
        }
        storage.verify_queue().suspect([file.id], now_ns());
        let report = storage.verify_pass().await;
        assert_eq!(
            (report.unavailable, report.quarantined),
            (1, 0),
            "{case:?}: {report:?}"
        );
        assert_eq!(
            storage.catalog_files(FileTable::Spans).len(),
            1,
            "{case:?}: the file left the catalog"
        );

        // Releasing also drops the truncations the pass did not use.
        chaos.release();
        let report = storage.verify_pass().await;
        assert_eq!(report.healthy, 1, "{case:?}: {report:?}");
        assert_eq!(report.quarantined, 0, "{case:?}");
        storage.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_put_under_faults_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    let storage = open(config(dir.path()), &chaos).await.unwrap();

    let bytes = Bytes::from_static(b"a blob whose put is ambiguous");
    let sha256: [u8; 32] = Sha256::digest(bytes.as_ref()).into();
    let blob = || MediaBlob {
        sha256,
        mime_type: "text/plain".to_owned(),
        bytes: bytes.clone(),
    };
    let sha = hex::encode(sha256);
    let path = Objects::media_path(project(), &sha);

    // The object is written, then the call fails: the ambiguous upload.
    chaos.push(Call::Put, Fault::After(Status::SlowDown));
    assert!(storage.put_media(project(), blob()).await.is_err());
    assert!(exists(&inner, &path).await, "the failed put wrote nothing");
    assert!(
        storage.staged_paths().await.unwrap().contains(&path),
        "the blob's path is not staged"
    );

    // A second put of the same blob overwrites it (spec 03 §9).
    storage.put_media(project(), blob()).await.unwrap();
    storage.collect_staged(Duration::ZERO).await.unwrap();
    assert!(storage.staged_paths().await.unwrap().is_empty());

    let audit = collect_orphans_dry_run(dir.path(), inner.as_ref()).await;
    assert_eq!(
        (audit.orphans, audit.missing, audit.missing_media),
        (0, 0, 0),
        "{audit:?}"
    );
    let read = storage.get_media(project(), &sha256, None).await.unwrap();
    assert_eq!(read.bytes, bytes);
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flush_retries_back_off_while_the_store_fails() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    chaos.hold(&[Call::Put, Call::MultipartStart]);
    let mut config = config(dir.path());
    config.flush.upload_concurrency = 1;
    let storage = open(config, &chaos).await.unwrap();

    // One upload at a time: each attempt makes exactly one upload call, for the spans
    // file, and the attempt returns before the traces file.
    let started = Instant::now();
    storage
        .append_spans(project(), spans_at(&[1]))
        .await
        .unwrap();
    tokio::time::sleep_until((started + Duration::from_millis(3_500)).into()).await;
    // The spans file is above the 16 KiB single-put size, so its attempt starts a
    // multipart upload: each attempt makes one of the two calls. The attempts are at
    // about 0, 1 and 3 s, so 2 or 3 of them are made in 3.5 s.
    let attempts = chaos.count(Call::Put) + chaos.count(Call::MultipartStart);
    assert!(
        (2..=3).contains(&attempts),
        "{attempts} attempts in 3.5 s: the backoff is not 1, 2, 4 s"
    );

    chaos.release();
    // The next attempt is due at about 7 s, 3.5 s after the release, so 6 s is
    // enough for it.
    let released = Instant::now();
    while storage.catalog_rows(Table::Spans) != 1 {
        assert!(
            released.elapsed() < Duration::from_secs(6),
            "the flush did not commit after the store recovered"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The failed attempts left their names staged: the spans and traces files
    // of at least one attempt.
    assert!(storage.staged_paths().await.unwrap().len() >= 2);

    let shutdown = Instant::now();
    storage.shutdown().await.unwrap();
    assert!(
        shutdown.elapsed() < Duration::from_secs(35),
        "shutdown took {:?}",
        shutdown.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_retries_a_failed_delete() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    let storage = open(config(dir.path()), &chaos).await.unwrap();

    // Two L0 spans files in one partition, then a compaction that obsoletes them.
    for index in 0..2_u8 {
        let base = at(10, 0, 0) + i64::from(index) * 60 * 1_000_000_000;
        let batch = span_rows(&[span_row(0, 10 + index, base, base + 1_000_000_000)]);
        let committed = storage.catalog_rows(Table::Spans);
        storage.append_spans(project(), batch).await.unwrap();
        wait_for_catalog(&storage, Table::Spans, committed + 1).await;
    }
    storage.compact(at(11, 0, 0)).await.unwrap();

    let obsolete = obsolete_paths(dir.path()).await;
    let n = obsolete.len();
    assert!(n >= 2, "compaction obsoleted {n} files");

    chaos.push(Call::Delete, Fault::After(Status::SlowDown));
    assert!(
        storage.collect_obsolete(Duration::ZERO).await.is_err(),
        "the failed delete did not fail the collection"
    );
    // The first delete took effect, and its row stays obsolete.
    assert_eq!(obsolete_paths(dir.path()).await.len(), n);
    let mut present = 0;
    for path in &obsolete {
        if exists(&inner, path).await {
            present += 1;
        }
    }
    assert_eq!(present, n - 1, "{n} obsolete objects, {present} left");

    let report = storage.collect_obsolete(Duration::ZERO).await.unwrap();
    assert_eq!(report.deleted, u64::try_from(n).unwrap(), "{report:?}");
    assert!(obsolete_paths(dir.path()).await.is_empty());
    let audit = collect_orphans_dry_run(dir.path(), inner.as_ref()).await;
    assert_eq!(audit.orphans, 0, "{audit:?}");
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_collection_continues_after_one_failed_delete() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    // Two failed attempts, each staging a spans file and a traces file.
    chaos.push(Call::Put, Fault::Before(Status::SlowDown));
    chaos.push(Call::Put, Fault::Before(Status::SlowDown));
    let mut config = config(dir.path());
    config.flush.upload_concurrency = 1;
    let storage = open(config, &chaos).await.unwrap();
    storage
        .append_spans(project(), spans_at(&[1]))
        .await
        .unwrap();
    wait_for_catalog(&storage, Table::Spans, 1).await;
    let staged = storage.staged_paths().await.unwrap();
    assert_eq!(staged.len(), 4, "{staged:?}");

    chaos.push(Call::Delete, Fault::Before(Status::SlowDown));
    assert!(storage.collect_staged(Duration::ZERO).await.is_err());
    assert!(!storage.staged_paths().await.unwrap().is_empty());

    // The next pass collects whatever the failed one left.
    storage.collect_staged(Duration::ZERO).await.unwrap();
    assert!(storage.staged_paths().await.unwrap().is_empty());
    storage.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_read_does_not_stay_in_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(InMemory::new());
    let chaos = ChaosStore::new(Arc::clone(&inner) as _);
    let storage = open(config(dir.path()), &chaos).await.unwrap();
    storage
        .append_spans(project(), spans_at(&[1]))
        .await
        .unwrap();
    wait_for_catalog(&storage, Table::Spans, 1).await;
    let file = storage.catalog_files(FileTable::Spans)[0].clone();
    let path = ObjectPath::from(file.path.as_str());
    let name = cached_name(&path).expect("a data file has a cache name");
    storage
        .objects
        .read_cache()
        .expect("the read cache is on")
        .evict_file(&name);

    // One corrupt byte in the first read from the store. The read may fail, or
    // return rows: the byte may fall outside what the query decodes.
    chaos.push(Call::Get, Fault::Corrupt);
    let _ = span_count(&storage).await;

    // Verification reads the store, not the cache, so the file is healthy.
    storage.verify_queue().suspect([file.id], now_ns());
    let report = storage.verify_pass().await;
    assert_eq!(report.healthy, 1, "{report:?}");

    // Whatever the corrupt read cached must not outlive the healthy verdict.
    let cached = storage
        .objects
        .get_range(&file.path, Some(0..file.size))
        .await
        .unwrap();
    let stored = inner.get(&path).await.unwrap().bytes().await.unwrap();
    assert!(
        cached == stored,
        "the cache still holds a corrupt block: {} cached bytes differ from the object",
        cached.len()
    );
    assert_eq!(span_count(&storage).await.unwrap(), 1);
    storage.shutdown().await.unwrap();
}
