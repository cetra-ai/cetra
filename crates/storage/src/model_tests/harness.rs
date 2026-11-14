//! Lives of one `Storage` for the model test: clean restarts and in-process
//! crashes, each life on a tokio runtime of its own (D-R-4).

use std::sync::Arc;
use std::time::Duration;

use bytesize::ByteSize;
use chrono::{Datelike, NaiveDate};
use datafusion::execution::context::{SessionConfig, SessionContext};
use llmobs_core::ids::ProjectId;

use crate::Storage;
use crate::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use crate::test_support::{span_row, span_rows};
use crate::wal::Table;

const PROJECT_A: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000a1";

/// Day `D`, 2026-03-05 (UTC).
const D: NaiveDate = match NaiveDate::from_ymd_opt(2026, 3, 5) {
    Some(date) => date,
    None => panic!("day D is a valid date"),
};

const DAY_NS: i64 = 86_400_000_000_000;
const HOUR_NS: i64 = 3_600_000_000_000;

/// Nanoseconds since the Unix epoch at 00:00 UTC of `date`.
fn midnight(date: NaiveDate) -> i64 {
    (i64::from(date.num_days_from_ce()) - 719_163) * DAY_NS
}

fn project() -> ProjectId {
    PROJECT_A.parse().expect("PROJECT_A parses as a project id")
}

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
    /// Kept so the directory lives as long as the harness.
    dir: tempfile::TempDir,
    config: StorageConfig,
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
    /// A fresh temp dir and `model_config(dir)`, opened.
    pub(super) fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir for a model case");
        let config = model_config(dir.path());
        let life = Some(open_life(&config));
        Self { dir, config, life }
    }

    pub(super) fn dir(&self) -> &std::path::Path {
        self.dir.path()
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
    let start = midnight(D) + 10 * HOUR_NS;
    let ingest = midnight(D) + 12 * HOUR_NS;
    let spans: Vec<_> = (first_span..first_span + count)
        .map(|span| {
            let mut row = span_row(1, span, start, ingest);
            row.project_id = project();
            row
        })
        .collect();
    harness
        .block_on(harness.storage().append_spans(project(), span_rows(&spans)))
        .expect("append_spans on project A");
}

/// Rows of the spans table that a read of `PROJECT_A` sees.
fn visible_span_rows(harness: &Harness) -> usize {
    harness.block_on(async {
        let storage = harness.storage();
        let query = storage
            .query(project(), Default::default())
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
