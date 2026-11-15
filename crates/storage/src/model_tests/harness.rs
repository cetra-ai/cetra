//! Lives of one `Storage` for the model test: clean restarts and in-process
//! crashes, each life on a tokio runtime of its own (D-R-4).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use bytes::Bytes;
use bytesize::ByteSize;
use datafusion::execution::context::{SessionConfig, SessionContext};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

use llmobs_core::ids::ProjectId;

use super::shape::{HOUR_NS, P, day, midnight};
use crate::Storage;
use crate::catalog::FileMeta;
use crate::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use crate::error::OpenError;
use crate::fs::lossy::{CrashReport, Lossy};
use crate::objects::PutSizes;
use crate::test_support::{ChaosStore, span_row, span_rows};
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

/// The sizes that make the model's files take both upload paths (R4.4): a file of
/// more than 16 KiB is multipart, in parts of 8 KiB.
pub(super) const MODEL_PUT_SIZES: PutSizes = PutSizes {
    single_max: 16 * 1024,
    part: 8 * 1024,
};

/// The sizes of a run against a real bucket (R4.8). Every part but the last must be at least 5 MiB
/// (`EntityTooSmall` otherwise), so the parts are 5 MiB; a file above 16 KiB is still multipart, as
/// one last part.
pub(super) const REMOTE_PUT_SIZES: PutSizes = PutSizes {
    single_max: 16 * 1024,
    part: 5 * 1024 * 1024,
};

/// What S3 mode changes in `model_config` (R4.4): the read cache is on, so every data-file read
/// goes through it, and uploads and compaction run one at a time, so most calls are sequential and
/// a seed usually replays. The data directory keeps the WAL and the catalog.
pub(super) fn s3_tweaks(config: &mut StorageConfig) {
    config.query.read_cache_bytes = Some(ByteSize::mib(64));
    config.query.target_partitions = 1;
    config.flush.upload_concurrency = 1;
    config.compaction.threads = 1;
}

/// `model_config` with every background task running often (R6.5): flushes, and compaction,
/// deletion and verify passes, at the minimums `StorageConfig::validate` allows. GC keeps its
/// interval, so the GC thread of the concurrent runner drives it.
pub(super) fn concurrent_tweaks(config: &mut StorageConfig) {
    config.flush.max_age = Duration::from_millis(100);
    config.compaction.interval = Duration::from_millis(100);
    config.compaction.l0_files = 2;
    config.compaction.l0_age = Duration::from_secs(1);
    config.compaction.threads = 2;
    config.gc.grace = Duration::from_secs(1);
}

/// Tokio worker threads of a life when [`HarnessOptions::workers`] is 0.
const WORKERS: usize = 2;

/// How a harness opens its lives (R5.1). `Default`: the system temp dir, local objects, 2 workers.
#[derive(Default)]
pub(super) struct HarnessOptions {
    /// Parent of the data directory; `None` is the system temp dir.
    pub(super) root: Option<std::path::PathBuf>,
    /// The object store every life opens with, and the sizes its uploads switch to multipart at
    /// (`Storage::open_with_store`). `None` opens local objects. The store is wrapped in a
    /// [`ChaosStore`] with no schedule, so faults can be injected into it.
    pub(super) store: Option<(Arc<dyn ObjectStore>, PutSizes)>,
    /// Tokio worker threads per life; 0 means [`WORKERS`].
    pub(super) workers: usize,
    /// No lossy root: the files are not recorded, so the disk holds only what storage keeps. The
    /// lossy shadow keeps a hard link of every file the storage unlinks, so on a small filesystem
    /// it would fill the disk with WAL segments that are already truncated (R5.4, D7). A lossy
    /// crash is not possible in an unrecorded harness.
    pub(super) unrecorded: bool,
}

/// The remote store of a harness with an object store.
pub(super) struct Remote {
    /// What storage writes to: faults are injected here.
    pub(super) chaos: ChaosStore,
    /// Passed to `Storage::open_with_store` at every open.
    pub(super) put_sizes: PutSizes,
}

/// Field order matters only for an implicit drop; `restart` and `crash`
/// always drop `storage` explicitly before shutting `runtime` down.
pub(super) struct Life {
    storage: Storage,
    runtime: tokio::runtime::Runtime,
}

pub(super) struct Harness {
    /// Kept so a harness made by `with_options` owns its directory. The data
    /// directory is `data` below it, and the lossy shadow is `shadow`. An
    /// `attach`ed harness borrows its directory from the caller.
    _dir: Option<tempfile::TempDir>,
    data: std::path::PathBuf,
    config: StorageConfig,
    /// The data directory's lossy root (R2.2), registered before the first open. `None` when the
    /// harness is unrecorded (R5.4): its files are not shadowed, so a lossy crash is not possible.
    lossy: Option<Lossy>,
    life: Option<Life>,
    /// What each `lossy_crash` of this case did, in order.
    crashes: Vec<CrashReport>,
    /// `None` in local mode.
    remote: Option<Remote>,
    /// Tokio worker threads per life.
    workers: usize,
    /// The `list` calls the orphan audits made (S3 mode). `check_full` expects exactly these
    /// and no others: storage never lists the bucket (spec 03 §5).
    pub(super) audit_lists: u64,
    /// The `FaultyRead` ops of this case that failed under an injected fault (S3 mode, R4.5).
    pub(super) faulty_read_errors: AtomicU64,
}

/// How long a shutdown runs before the ending window opens (R4.5). `Storage::shutdown` cuts its
/// flush off after 30 s, and the window must already be open by then.
const SHUTDOWN_WINDOW: Duration = Duration::from_secs(25);

/// How long a shutdown runs under the fault schedule before the schedule is switched off. A flush
/// that keeps failing then commits on its next retry, instead of sleeping its full backoff until
/// the 30 s cut-off.
const CALM_AFTER: Duration = Duration::from_secs(3);

/// Opens storage on `config`. With a store, it is `remote`'s chaos store. Opening runs calm:
/// the schedule is off while the open runs, and the previous setting is restored after it (a
/// store error during the open fails it by design, R4.6).
fn open_life(config: &StorageConfig, remote: Option<&Remote>, workers: usize) -> Life {
    let (runtime, opened) = try_open_life(config, remote, workers);
    let storage = opened.expect("open storage for a new life");
    Life { storage, runtime }
}

/// [`open_life`] without the panic: the new life's runtime comes back with the open's result, so
/// a failed open can still shut its runtime down (R5.3).
fn try_open_life(
    config: &StorageConfig,
    remote: Option<&Remote>,
    workers: usize,
) -> (tokio::runtime::Runtime, Result<Storage, OpenError>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("tokio runtime for a new life");
    let opened = match remote {
        None => runtime.block_on(Storage::open(config.clone())),
        Some(remote) => {
            let was_active = remote.chaos.is_active();
            remote.chaos.set_active(false);
            let opened = runtime.block_on(Storage::open_with_store(
                config.clone(),
                Arc::new(remote.chaos.clone()),
                remote.put_sizes,
            ));
            remote.chaos.set_active(was_active);
            opened
        }
    };
    (runtime, opened)
}

impl Harness {
    /// A fresh temp dir (under `options.root` when set), its data directory in lossy mode, and
    /// `model_config(data)` with `adjust` applied, opened. `adjust` runs after the store is chosen,
    /// so it can make S3 mode's changes (`s3_tweaks`).
    pub(super) fn with_options(
        options: HarnessOptions,
        adjust: impl FnOnce(&mut StorageConfig),
    ) -> Self {
        let dir = match &options.root {
            Some(root) => tempfile::TempDir::new_in(root).expect("temp dir under the test root"),
            None => tempfile::TempDir::new().expect("temp dir for a model case"),
        };
        let workers = if options.workers == 0 {
            WORKERS
        } else {
            options.workers
        };
        let mut harness = Self::open_in(
            &dir.path().join("data"),
            &dir.path().join("shadow"),
            options.store,
            workers,
            !options.unrecorded,
            adjust,
        );
        harness._dir = Some(dir);
        harness
    }

    /// `with_options` with the defaults and `change` applied: `wal.sync = "always"`.
    pub(super) fn new() -> Self {
        Self::with_options(HarnessOptions::default(), |_| {})
    }

    /// Opens the existing directory `data` (created when missing) in lossy mode, with the shadow
    /// at `shadow`, as the default config (`wal.sync = "always"`). The caller keeps both paths
    /// alive. With `seed`, a hook abort of this process first applies `crash(seed)` (R2.11).
    pub(super) fn attach(
        data: &std::path::Path,
        shadow: &std::path::Path,
        seed: Option<u64>,
    ) -> Self {
        let harness = Self::open_in(data, shadow, None, WORKERS, true, |_| {});
        if let Some(seed) = seed {
            harness
                .lossy
                .as_ref()
                .expect("an attached harness is recorded")
                .crash_on_abort(seed);
        }
        harness
    }

    /// Registers `data` as a lossy root with the shadow at `shadow`, and opens it under
    /// `model_config(data)` with `change` applied. A `store` is wrapped in a chaos store and
    /// opened with its put sizes. The caller sets `_dir` when it owns the paths.
    fn open_in(
        data: &std::path::Path,
        shadow: &std::path::Path,
        store: Option<(Arc<dyn ObjectStore>, PutSizes)>,
        workers: usize,
        recorded: bool,
        change: impl FnOnce(&mut StorageConfig),
    ) -> Self {
        let lossy = recorded.then(|| Lossy::enable(data, shadow));
        let remote = store.map(|(inner, put_sizes)| Remote {
            chaos: ChaosStore::new(inner),
            put_sizes,
        });
        let mut config = model_config(data);
        change(&mut config);
        let life = Some(open_life(&config, remote.as_ref(), workers));
        Self {
            _dir: None,
            data: data.to_path_buf(),
            config,
            lossy,
            life,
            crashes: Vec::new(),
            remote,
            workers,
            audit_lists: 0,
            faulty_read_errors: AtomicU64::new(0),
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

    /// Marks the window in which a life ends (S3 mode): an upload cancelled there is abandoned,
    /// not leaked (R4.2). Called just before the old storage ends, and cleared once its runtime
    /// has shut down.
    fn set_ending(&self, on: bool) {
        if let Some(remote) = &self.remote {
            remote.chaos.set_ending(on);
        }
    }

    /// Clean shutdown, then a new life. The ending window opens only once `shutdown` has run for
    /// [`SHUTDOWN_WINDOW`] (R4.5): the flush of a shutdown writes and uploads under the schedule,
    /// and an upload it drops during a clean shutdown is a leak, not an abandoned one. The
    /// window still opens in time for shutdown's own 30 s cut-off, which cancels uploads by design.
    pub(super) fn restart(&mut self) {
        self.shut_down_clean();
        self.life = Some(open_life(&self.config, self.remote.as_ref(), self.workers));
    }

    /// Clean shutdown, `adjust` applied to the config, then a new life with it (R5.4: the read
    /// cache of D7 is off while the data is seeded and on in the second life). Later restarts
    /// keep the changed config.
    pub(super) fn restart_with(&mut self, adjust: impl FnOnce(&mut StorageConfig)) {
        self.shut_down_clean();
        adjust(&mut self.config);
        self.life = Some(open_life(&self.config, self.remote.as_ref(), self.workers));
    }

    /// The shutdown of [`Self::restart`]: it leaves no life.
    fn shut_down_clean(&mut self) {
        let Life { storage, runtime } = self.life.take().expect("a life");
        let remote = self.remote.as_ref();
        let result = runtime.block_on(async {
            let started = tokio::time::Instant::now();
            let shutdown = storage.shutdown();
            tokio::pin!(shutdown);
            tokio::select! {
                result = &mut shutdown => result,
                () = tokio::time::sleep(CALM_AFTER) => {
                    if let Some(remote) = remote {
                        remote.chaos.set_active(false);
                    }
                    tokio::select! {
                        result = &mut shutdown => result,
                        () = tokio::time::sleep_until(started + SHUTDOWN_WINDOW) => {
                            if let Some(remote) = remote {
                                remote.chaos.set_ending(true);
                            }
                            shutdown.await
                        }
                    }
                }
            }
        });
        result.expect("clean shutdown");
        self.set_ending(true);
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.set_ending(false);
    }

    /// Drop without shutdown, then a new life.
    pub(super) fn crash(&mut self) {
        let Life { storage, runtime } = self.life.take().expect("a life");
        self.set_ending(true);
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.set_ending(false);
        self.life = Some(open_life(&self.config, self.remote.as_ref(), self.workers));
    }

    /// Drop without shutdown, then try to open a new life. On `Err` there is no life until
    /// [`Self::open_again`] (R5.3: a full disk can refuse the open).
    pub(super) fn crash_then_try_open(&mut self) -> Result<(), OpenError> {
        let Life { storage, runtime } = self.life.take().expect("a life");
        self.set_ending(true);
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.set_ending(false);
        let (runtime, opened) = try_open_life(&self.config, self.remote.as_ref(), self.workers);
        match opened {
            Ok(storage) => {
                self.life = Some(Life { storage, runtime });
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_timeout(Duration::from_secs(10));
                Err(error)
            }
        }
    }

    /// Opens a life when there is none, after a failed [`Self::crash_then_try_open`].
    pub(super) fn open_again(&mut self) {
        assert!(
            self.life.is_none(),
            "open_again is for a harness whose open failed: it has a life"
        );
        self.life = Some(open_life(&self.config, self.remote.as_ref(), self.workers));
    }

    /// Drop without shutdown, apply a lossy crash with `seed` to the data directory, then a new
    /// life. The storage is closed before the crash, as `Lossy::crash_with` requires. The report
    /// is kept for [`Self::crashes`]. Lossy crashes cover local files only, so this is local mode
    /// only (R4.4).
    pub(super) fn lossy_crash(&mut self, seed: u64) {
        assert!(
            self.remote.is_none(),
            "a lossy crash covers local files only, not a harness with an object store"
        );
        let Life { storage, runtime } = self.life.take().expect("a life");
        let lossy = self
            .lossy
            .as_ref()
            .expect("a lossy crash needs a recorded harness");
        // The storage's drop syncs what the WAL writer still holds. A process that crashes
        // here would not sync it, so the shim halts first: the drop and the shutdown of the
        // runtime then cannot make the tail durable, whichever order they run in.
        lossy.halt();
        drop(storage);
        runtime.shutdown_timeout(Duration::from_secs(10));
        self.crashes.push(lossy.crash(seed));
        self.life = Some(open_life(&self.config, self.remote.as_ref(), self.workers));
    }

    /// The reports of this case's lossy crashes, oldest first.
    pub(super) fn crashes(&self) -> &[CrashReport] {
        &self.crashes
    }

    /// The remote store's chaos store; `None` in local mode.
    pub(super) fn chaos(&self) -> Option<&ChaosStore> {
        self.remote.as_ref().map(|remote| &remote.chaos)
    }

    /// The bytes of `file`, read past the chaos store and the read cache: from
    /// `<dir>/objects/<path>` in local mode, from the inner store in S3 mode (R4.4).
    pub(super) fn object_bytes(&self, file: &FileMeta) -> Bytes {
        match &self.remote {
            None => Bytes::from(
                std::fs::read(self.data.join("objects").join(&file.path))
                    .expect("read a catalogued object from disk"),
            ),
            Some(remote) => {
                let key = ObjectPath::parse(&file.path).expect("a catalogued object path");
                self.block_on(async {
                    remote
                        .chaos
                        .inner()
                        .get(&key)
                        .await
                        .expect("get a catalogued object from the store")
                        .bytes()
                        .await
                        .expect("read the body of a catalogued object")
                })
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(Life { storage, runtime }) = self.life.take() {
            // Shutdown must not meet faults (R4.5): the schedule is off before the storage closes.
            if let Some(chaos) = self.chaos() {
                chaos.set_active(false);
            }
            self.set_ending(true);
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

/// Rows of the spans table that a read of `project` sees.
pub(super) fn visible_span_rows(harness: &Harness, project: ProjectId) -> usize {
    harness.block_on(async {
        let storage = harness.storage();
        let query = storage
            .query(project, Default::default())
            .await
            .expect("query on the project");
        let ctx =
            SessionContext::new_with_config_rt(SessionConfig::new(), Arc::clone(&query.runtime));
        ctx.register_table("spans_t", Arc::clone(&query.spans))
            .expect("register spans of the project");
        let batches = ctx
            .sql("SELECT trace_id FROM spans_t")
            .await
            .expect("plan spans read of the project")
            .collect()
            .await
            .expect("run spans read of the project");
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
    assert_eq!(visible_span_rows(&harness, P::A.id()), 3);
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
    assert_eq!(visible_span_rows(&harness, P::A.id()), 5);
}

#[test]
fn harness_is_sync() {
    fn assert_sync<T: Sync>() {}
    assert_sync::<Harness>();
}
