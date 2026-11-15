//! Lives of one `Storage` for the model test: clean restarts and in-process
//! crashes, each life on a tokio runtime of its own (D-R-4).

use std::sync::Arc;
use std::time::Duration;

use bytesize::ByteSize;
use datafusion::execution::context::{SessionConfig, SessionContext};

use super::shape::{HOUR_NS, P, day, midnight};
use crate::Storage;
use crate::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use crate::fs::lossy::{CrashReport, Lossy};
use crate::test_support::{span_row, span_rows};
use crate::wal::Table;

/// The storage config every model case uses.
pub(super) fn model_config(dir: &std::path::Path) -> StorageConfig {
    StorageConfig {
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig::default(),
        flush: FlushConfig {
            max_age: Duration::from_secs(3_600),
            ..FlushConfig::default()
        },
        memtable: MemtableConfig::default(),
        query: QueryConfig::default(),
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            interval: Duration::from_secs(3_600),
            l0_files: 2,
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

/// Field order matters only for an implicit drop; `restart` and `crash`
/// always drop `storage` explicitly before shutting `runtime` down.
pub(super) struct Life {
    storage: Storage,
    runtime: tokio::runtime::Runtime,
}

pub(super) struct Harness {
    /// Kept so a harness made by `with_config` owns its directory. The data
    /// directory is `data` below it, and the lossy shadow is `shadow`. An
    /// `attach`ed harness borrows its directory from the caller.
    _dir: Option<tempfile::TempDir>,
    data: std::path::PathBuf,
    config: StorageConfig,
    /// The data directory's lossy root (R2.2), registered before the first open.
    lossy: Lossy,
    life: Option<Life>,
}

fn open_life(config: &StorageConfig) -> Life {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for a new life");
    let storage = runtime
        .block_on(Storage::open(config.clone()))
        .expect("open storage for a new life");
    Life { storage, runtime }
}

impl Harness {
    /// A fresh temp dir, its data directory in lossy mode, and `model_config(data)` with `change`
    /// applied, opened.
    pub(super) fn with_config(change: impl FnOnce(&mut StorageConfig)) -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir for a model case");
        let mut harness =
            Self::open_in(&dir.path().join("data"), &dir.path().join("shadow"), change);
        harness._dir = Some(dir);
        harness
    }

    /// `with_config` with no change: `wal.sync = "always"`.
    pub(super) fn new() -> Self {
        Self::with_config(|_| {})
    }

    /// Opens the existing directory `data` (created when missing) in lossy mode, with the shadow
    /// at `shadow`, as the default config (`wal.sync = "always"`). The caller keeps both paths
    /// alive. With `seed`, a hook abort of this process first applies `crash(seed)` (R2.11).
    pub(super) fn attach(
        data: &std::path::Path,
        shadow: &std::path::Path,
        seed: Option<u64>,
    ) -> Self {
        let harness = Self::open_in(data, shadow, |_| {});
        if let Some(seed) = seed {
            harness.lossy.crash_on_abort(seed);
        }
        harness
    }

    /// Registers `data` as a lossy root with the shadow at `shadow`, and opens it under
    /// `model_config(data)` with `change` applied. The caller sets `_dir` when it owns the paths.
    fn open_in(
        data: &std::path::Path,
        shadow: &std::path::Path,
        change: impl FnOnce(&mut StorageConfig),
    ) -> Self {
        let lossy = Lossy::enable(data, shadow);
        let mut config = model_config(data);
        change(&mut config);
        let life = Some(open_life(&config));
        Self {
            _dir: None,
            data: data.to_path_buf(),
            config,
            lossy,
            life,
        }
    }

    pub(super) fn dir(&self) -> &std::path::Path {
        &self.data
    }

    pub(super) fn config(&self) -> &StorageConfig {
        &self.config
    }

    pub(super) fn storage(&self) -> &Storage {
        &self.life.as_ref().expect("a life").storage
    }

    /// Runs `future` on the current life's runtime.
    pub(super) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.life.as_ref().expect("a life").runtime.block_on(future)
    }

    /// Clean shutdown, then a new life.
    pub(super) fn restart(&mut self) {
        let Life { storage, runtime } = self.life.take().expect("a life");
        runtime
            .block_on(storage.shutdown())
            .expect("clean shutdown");
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.life = Some(open_life(&self.config));
    }

    /// Drop without shutdown, then a new life.
    pub(super) fn crash(&mut self) {
        let Life { storage, runtime } = self.life.take().expect("a life");
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.life = Some(open_life(&self.config));
    }

    /// Drop without shutdown, apply a lossy crash with `seed` to the data directory, then a new
    /// life. The storage is closed before the crash, as `Lossy::crash_with` requires.
    pub(super) fn lossy_crash(&mut self, seed: u64) -> CrashReport {
        let Life { storage, runtime } = self.life.take().expect("a life");
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        let report = self.lossy.crash(seed);
        self.life = Some(open_life(&self.config));
        report
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(Life { storage, runtime }) = self.life.take() {
            let _ = runtime.block_on(storage.shutdown());
            drop(storage);
            runtime.shutdown_timeout(Duration::from_secs(10));
        }
    }
}

/// Appends `count` spans of trace 1 to `PROJECT_A`, numbered from `first_span`.
/// Every row has the project set, as `append_spans` requires.
fn append_spans_of_trace_one(harness: &Harness, first_span: u8, count: u8) {
    let start = midnight(day(0)) + 10 * HOUR_NS;
    let ingest = midnight(day(0)) + 12 * HOUR_NS;
    let spans: Vec<_> = (first_span..first_span + count)
        .map(|span| {
            let mut row = span_row(1, span, start, ingest);
            row.project_id = P::A.id();
            row
        })
        .collect();
    harness
        .block_on(harness.storage().append_spans(P::A.id(), span_rows(&spans)))
        .expect("append_spans on project A");
}

/// Rows of the spans table that a read of `PROJECT_A` sees.
fn visible_span_rows(harness: &Harness) -> usize {
    harness.block_on(async {
        let storage = harness.storage();
        let query = storage
            .query(P::A.id(), Default::default())
            .await
            .expect("query on project A");
        let ctx =
            SessionContext::new_with_config_rt(SessionConfig::new(), Arc::clone(&query.runtime));
        ctx.register_table("spans_t", Arc::clone(&query.spans))
            .expect("register spans on project A");
        let batches = ctx
            .sql("SELECT trace_id FROM spans_t")
            .await
            .expect("plan spans read on project A")
            .collect()
            .await
            .expect("run spans read on project A");
        batches.iter().map(|batch| batch.num_rows()).sum()
    })
}

#[test]
fn crash_keeps_acknowledged_appends() {
    let mut harness = Harness::new();
    append_spans_of_trace_one(&harness, 0, 3);
    assert!(
        harness.storage().catalog_files(Table::Spans).is_empty(),
        "nothing flushed before the crash"
    );
    harness.crash();
    assert_eq!(visible_span_rows(&harness), 3);
}

#[test]
fn crash_then_restart_flushes_the_replayed_rows() {
    let mut harness = Harness::new();
    append_spans_of_trace_one(&harness, 0, 3);
    harness.crash();
    assert!(
        harness.storage().catalog_files(Table::Spans).is_empty(),
        "the replayed rows are still in the memtable"
    );
    harness.restart();

    let files = harness.storage().catalog_files(Table::Spans);
    assert!(!files.is_empty(), "restart flushed the replayed rows");
    let rows: u64 = files.iter().map(|file| file.rows).sum();
    assert_eq!(rows, 3);
    assert_eq!(harness.storage().published_rows(Table::Spans), 0);
}

#[test]
fn restart_after_crash_twice() {
    let mut harness = Harness::new();
    append_spans_of_trace_one(&harness, 0, 3);
    harness.crash();
    harness.crash();
    append_spans_of_trace_one(&harness, 3, 2);
    harness.restart();
    assert_eq!(visible_span_rows(&harness), 5);
}
