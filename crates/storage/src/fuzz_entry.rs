//! Fuzz entry points (track R M3, docs/plans/r-hardening/03-m3-fuzzing.md). Each one takes arbitrary bytes and must not panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arbitrary::{Arbitrary, Unstructured};
use bytes::Bytes;
use cetra_core::ids::ProjectId;
use cetra_core::schema::{scores, spans};
use chrono::NaiveDate;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, SqliteConnection};
use uuid::Uuid;

use crate::anchor::Anchors;
use crate::catalog::{
    Catalog, CatalogEdit, FileId, FileMeta, FileTable, check_file_sources, load,
    load_project_states, load_tombstones,
};
use crate::compaction::plan::{Policy, plan};
use crate::compaction::slice::time_values;
use crate::config::{CompactionConfig, WalConfig};
use crate::disk::Disk;
use crate::hooks::Hooks;
use crate::memtable::Memtable;
use crate::read_cache::{BLOCK, CacheStore, ReadCache};
use crate::read_state::ReadState;
use crate::verify::{VerifyQueue, verify_file};
use crate::wal::segment::segment_header;
use crate::wal::{Decoded, Table, decode_next, encode, push_framed, read_batch, segment_name};
use crate::{
    EngineContext, ListScope, ProjectQuery, QeRuntime, QueryContext, QueryOptions,
    ScoreListRequest, SessionListRequest, SessionSort, SortOrder, SpanListRequest, SpanSort,
    TimeWindow, TraceListRequest, TraceSort, ViewName, recover_table, replay_batch, score_list,
    session_list, span_list, trace_detail, trace_list, validate_filter,
};

/// Reads a fuzz input's header. Missing bytes read as zero, so every input is valid.
struct Cursor<'a> {
    data: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn u8(&mut self) -> u8 {
        self.bytes(1).first().copied().unwrap_or(0)
    }

    fn u32(&mut self) -> u32 {
        let mut buf = [0; 4];
        let bytes = self.bytes(4);
        buf[..bytes.len()].copy_from_slice(bytes);
        u32::from_le_bytes(buf)
    }

    fn u64(&mut self) -> u64 {
        let mut buf = [0; 8];
        let bytes = self.bytes(8);
        buf[..bytes.len()].copy_from_slice(bytes);
        u64::from_le_bytes(buf)
    }

    fn i64(&mut self) -> i64 {
        i64::from_le_bytes(self.u64().to_le_bytes())
    }

    /// Everything not read yet.
    fn rest(self) -> &'a [u8] {
        self.data
    }

    /// At most `n` bytes, fewer when the input is short.
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.data.len());
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        head
    }
}

/// A scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cetra-fuzz-{}", Uuid::now_v7()));
        // Harness setup, not input-dependent.
        std::fs::create_dir_all(&path).expect("create the fuzz scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Silences the panic hook that libFuzzer installs. That hook aborts on every
/// panic, including the ones our `catch_unwind` guards catch (spec 03 §10), so
/// a caught decoder panic would look like a crash. An uncaught panic still
/// aborts: the entries are called from libFuzzer's `extern "C"` wrapper, which
/// cannot unwind.
///
/// A no-op outside fuzzing builds, so `cargo test` keeps every other test's panic
/// message.
fn quiet_caught_panics() {
    #[cfg(fuzzing)]
    {
        static HOOK: OnceLock<()> = OnceLock::new();
        HOOK.get_or_init(|| std::panic::set_hook(Box::new(|_| {})));
    }
}

/// Frames arbitrary bytes as WAL records and decodes them; the IPC payload is read as a batch.
pub fn wal_record(data: &[u8]) {
    quiet_caught_panics();
    let mut offset = 0;
    while let Decoded::Record { next, .. } = decode_next(data, offset) {
        assert!(next > offset, "framing must advance");
        offset = next;
    }
    for table in [Table::Spans, Table::Scores] {
        let Ok(batch) = read_batch(data, table) else {
            continue;
        };
        let _ = replay_batch(&mut Memtable::new(table), 1, batch.clone());

        let mut out = Vec::new();
        assert!(
            encode(1, &batch, &mut out).is_ok(),
            "a decoded batch re-encodes"
        );
        let Decoded::Record { payload, .. } = decode_next(&out, 0) else {
            panic!("a re-encoded record frames");
        };
        let again = read_batch(&out[payload], table).expect("a re-encoded batch decodes");
        assert_eq!(again.num_rows(), batch.num_rows());
        assert_eq!(again.schema().fields(), batch.schema().fields());
    }
}

/// What the `wal_replay` input says to do, after its segments are on disk.
struct Staged {
    table: Table,
    dir: PathBuf,
    after_seq: u64,
    skip_corrupt: bool,
}

/// Writes the input's segments into `scratch`, as `wal_replay` describes them.
fn stage(data: &[u8], scratch: &Scratch) -> Staged {
    let mut input = Cursor::new(data);
    let flags = input.u8();
    let table = if flags & 1 == 0 {
        Table::Spans
    } else {
        Table::Scores
    };
    let skip_corrupt = flags & 2 != 0;
    let segments = 1 + usize::from(input.u8()) % 3;
    let after_seq = input.u64();

    let dir = scratch.0.join(table.name());
    std::fs::create_dir_all(&dir).expect("create the WAL directory");
    for _ in 0..segments {
        let first_seq = input.u64();
        let mode = input.u8();
        let bytes = if mode & 1 == 0 {
            // A framed segment starts with its header; the raw branch stays headerless.
            let mut segment = segment_header().to_vec();
            let frames = usize::from(input.u8()) % 5;
            for _ in 0..frames {
                let seq = input.u64();
                let len = usize::try_from(input.u32())
                    .unwrap_or(usize::MAX)
                    .min(1 << 20);
                push_framed(&mut segment, seq, input.bytes(len));
            }
            let tear = usize::from(mode >> 1) % 32;
            segment.truncate(segment.len().saturating_sub(tear));
            segment
        } else {
            let len = usize::try_from(input.u32())
                .unwrap_or(usize::MAX)
                .min(1 << 20);
            input.bytes(len).to_vec()
        };
        std::fs::write(dir.join(segment_name(first_seq)), bytes)
            .expect("write a WAL segment into the scratch directory");
    }
    Staged {
        table,
        dir,
        after_seq,
        skip_corrupt,
    }
}

/// Writes the fuzzed segments into a scratch WAL directory and replays it twice.
///
/// The entry frames most segments itself with the real checksum; see the layout in
/// `03-m3-fuzzing.md` § R3.1.
pub fn wal_replay(data: &[u8]) {
    quiet_caught_panics();
    let scratch = Scratch::new();
    let staged = stage(data, &scratch);
    let segment_bytes = WalConfig::default().segment_bytes.0;
    // `Hooks` is a unit struct outside tests, where `default()` is the same value.
    #[allow(clippy::default_constructed_unit_structs)]
    let hooks = Hooks::default();
    let replay = |staged: &Staged| {
        recover_table(
            &staged.dir,
            staged.table,
            staged.after_seq,
            staged.skip_corrupt,
            segment_bytes,
            &hooks,
        )
    };
    let Ok(first) = replay(&staged) else {
        return;
    };
    let records = first.records;
    let memtable_bytes = first.table.memtable.bytes();
    // Dropping the first replay releases the new segment, which the second one reuses.
    drop(first);

    let second = replay(&staged).expect("a replay that truncated its tail replays again");
    assert_eq!(
        second.records, records,
        "a second replay sees the same records"
    );
    assert_eq!(
        second.table.memtable.bytes(),
        memtable_bytes,
        "a second replay rebuilds the same memtable"
    );
}

/// The one runtime the async entries share (track R M3). Replay tests call the
/// entries from many threads, and `block_on` on a multi-thread runtime allows that.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            // Harness setup, not input-dependent.
            .expect("build the fuzz runtime")
    })
}

/// One day in nanoseconds.
const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// The project of the seed rows (`test_support::span_row`), so they are not filtered out.
pub(crate) fn project() -> ProjectId {
    // Harness setup, not input-dependent.
    "018f6b4e-7c1a-7c1a-8c1a-000000000001"
        .parse()
        .expect("the seed project id parses")
}

/// The UTC day of the seed data: both `dataset_spans` and `dataset_scores` fall on it.
fn seed_day() -> NaiveDate {
    // Harness setup, not input-dependent.
    NaiveDate::from_ymd_opt(2026, 1, 1).expect("the seed day is a valid date")
}

/// Nanoseconds at midnight UTC of [`seed_day`].
fn seed_midnight() -> i64 {
    seed_day()
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp_nanos_opt()
        // Harness setup, not input-dependent.
        .expect("the seed day is in range")
}

/// The catalog row of the one file a Parquet entry reads, at its fixed path.
pub(crate) fn parquet_meta(table: FileTable, size: u64, footer_size: u64) -> FileMeta {
    FileMeta {
        id: FileId(Uuid::from_u128(1)),
        table,
        project: project(),
        day: seed_day(),
        level: 0,
        path: format!("tables/{}/fuzz.parquet", table.as_str()),
        size,
        footer_size,
        rows: 1,
        // Every row falls inside the file's range, so no pruning skips the file.
        min_time: i64::MIN,
        max_time: i64::MAX,
        min_ingest_time: i64::MIN,
        max_ingest_time: i64::MAX,
        fully_merged: false,
        deletions_applied_seq: 0,
        pack: None,
    }
}

fn fuzz_qe_runtime() -> Arc<QeRuntime> {
    static FUZZ_RUNTIME: OnceLock<Arc<QeRuntime>> = OnceLock::new();
    Arc::clone(FUZZ_RUNTIME.get_or_init(|| QeRuntime::for_test_with_cache(0)))
}

/// The read state over one file and the query context a read runs in. `store` must
/// already hold the file at `meta.path`. Harness setup: the inputs are fixed, so every
/// `expect` here is one.
pub(crate) fn parquet_query(
    store: Arc<dyn ObjectStore>,
    meta: &FileMeta,
) -> (QueryContext, Arc<VerifyQueue>) {
    let edit = CatalogEdit {
        add: vec![meta.clone()],
        ..CatalogEdit::default()
    };
    let (catalog, _, _) = Catalog::empty()
        .apply(&edit)
        .expect("one new file applies to an empty catalog");
    let state = ReadState {
        catalog: Arc::new(catalog),
        ..ReadState::empty()
    };
    let verify = Arc::new(VerifyQueue::default());
    let engine = EngineContext::from_state(
        &state,
        store,
        project(),
        &QueryOptions::new(None, None),
        fuzz_qe_runtime(),
        DAY_NS,
        &verify,
        runtime().handle().clone(),
    )
    .expect("a query over one file plans");
    let ctx = QueryContext::new(ProjectQuery::from_engine(engine), None)
        .expect("the query context builds");
    (ctx, verify)
}

/// A list scope over `window` (or all time), with the default exclusions off.
fn scope(window: Option<TimeWindow>) -> ListScope {
    ListScope {
        window,
        include_eval: true,
        include_experiments: true,
    }
}

/// Decodes arbitrary bytes as one Parquet file of a table and reads it through every
/// read path (track R M3). Layout: byte 0 selects the table, bytes 1..5 the footer
/// hint, bytes 5..13 the time bound, the rest is the file.
pub fn parquet_file(data: &[u8]) {
    quiet_caught_panics();
    let mut input = Cursor::new(data);
    let table = match input.u8() % 3 {
        0 => FileTable::Spans,
        1 => FileTable::Traces,
        _ => FileTable::Scores,
    };
    let footer_hint = input.u32();
    let raw_bound = input.i64();
    let file = input.rest();
    if file.is_empty() {
        return;
    }
    // The windowed reads cover one day from this bound, which always overlaps the seed day.
    let bound = seed_midnight().saturating_add(raw_bound.rem_euclid(DAY_NS));
    let meta = parquet_meta(
        table,
        u64::try_from(file.len()).unwrap_or(u64::MAX),
        u64::from(footer_hint),
    );
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    runtime().block_on(async {
        store
            .put(
                &ObjectPath::from(meta.path.as_str()),
                PutPayload::from(Bytes::copy_from_slice(file)),
            )
            .await
            // Harness setup, not input-dependent.
            .expect("store the file in memory")
    });
    let (ctx, _) = parquet_query(Arc::clone(&store), &meta);
    let timeout = Duration::from_secs(30);
    let window = || TimeWindow::new(bound, bound.saturating_add(DAY_NS)).ok();
    runtime().block_on(async {
        let _ = span_list(
            &ctx,
            SpanListRequest {
                strict_exclusions: false,
                filter: None,
                search: None,
                scope: scope(None),
                sort: SpanSort::StartTime,
                order: SortOrder::Desc,
                limit: 100,
                cursor: None,
                timeout,
            },
        )
        .await;
        let _ = span_list(
            &ctx,
            SpanListRequest {
                strict_exclusions: false,
                filter: Some("status = 'error'"),
                search: None,
                scope: scope(window()),
                sort: SpanSort::StartTime,
                order: SortOrder::Desc,
                limit: 100,
                cursor: None,
                timeout,
            },
        )
        .await;
        let _ = trace_list(
            &ctx,
            TraceListRequest {
                strict_exclusions: false,
                filter: None,
                search: None,
                scope: scope(None),
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: 100,
                cursor: None,
                timeout,
            },
        )
        .await;
        let _ = trace_list(
            &ctx,
            TraceListRequest {
                strict_exclusions: false,
                filter: Some("status = 'error'"),
                search: None,
                scope: scope(window()),
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: 100,
                cursor: None,
                timeout,
            },
        )
        .await;
        let _ = trace_detail(&ctx, "00010001000100010001000100010001", timeout).await;
        let _ = score_list(
            &ctx,
            ScoreListRequest {
                evaluator_id: None,
                rule_id: None,
                queue_id: None,
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: None,
                source: None,
                window: None,
                limit: 100,
                cursor: None,
                timeout,
            },
        )
        .await;
        let _ = verify_file(&store, &meta).await;
        match table {
            FileTable::Traces => {
                let _ = Anchors::new(DAY_NS)
                    .rebuild_file(&meta, Arc::clone(&store), bound)
                    .await;
            }
            FileTable::Spans => {
                let files = [Arc::new(meta.clone())];
                let _ = time_values(Arc::clone(&store), &files, spans::START_TIME).await;
            }
            FileTable::Scores => {
                let files = [Arc::new(meta.clone())];
                let _ = time_values(Arc::clone(&store), &files, scores::CREATED_AT).await;
            }
        }
    });
}

/// Parses arbitrary text as a filter over one view and runs it (track R M3). Layout:
/// byte 0 selects the view, the rest must be UTF-8.
pub fn filter_dsl(data: &[u8]) {
    quiet_caught_panics();
    let Some((&view, text)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    let view = match view % 4 {
        0 => ViewName::Traces,
        1 => ViewName::Spans,
        2 => ViewName::Sessions,
        _ => ViewName::Scores,
    };
    let text = text.to_owned();
    // A tokio worker's stack size: the server parses filters on one.
    let worker = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || run_filter(view, &text))
        // Harness setup, not input-dependent.
        .expect("spawn the filter thread");
    if let Err(payload) = worker.join() {
        std::panic::resume_unwind(payload);
    }
}

/// Parses and applies one filter over empty tables.
fn run_filter(view: ViewName, text: &str) {
    let verify = Arc::new(VerifyQueue::default());
    let engine = EngineContext::from_state(
        &ReadState::empty(),
        Arc::new(InMemory::new()),
        project(),
        &QueryOptions::new(None, None),
        fuzz_qe_runtime(),
        DAY_NS,
        &verify,
        runtime().handle().clone(),
    )
    // Harness setup, not input-dependent.
    .expect("a query over empty tables plans");
    let ctx = QueryContext::new(ProjectQuery::from_engine(engine), None)
        // Harness setup, not input-dependent.
        .expect("the query context builds");
    if validate_filter(&ctx, view, text).is_err() {
        return;
    }
    runtime().block_on(async {
        let timeout = Duration::from_secs(5);
        match view {
            ViewName::Traces => {
                let _ = trace_list(
                    &ctx,
                    TraceListRequest {
                        filter: Some(text),
                        search: None,
                        scope: scope(None),
                        strict_exclusions: false,
                        sort: TraceSort::StartTime,
                        order: SortOrder::Desc,
                        limit: 10,
                        cursor: None,
                        timeout,
                    },
                )
                .await;
            }
            ViewName::Spans => {
                let _ = span_list(
                    &ctx,
                    SpanListRequest {
                        filter: Some(text),
                        search: None,
                        scope: scope(None),
                        strict_exclusions: false,
                        sort: SpanSort::StartTime,
                        order: SortOrder::Desc,
                        limit: 10,
                        cursor: None,
                        timeout,
                    },
                )
                .await;
            }
            ViewName::Sessions => {
                let _ = session_list(
                    &ctx,
                    SessionListRequest {
                        filter: Some(text),
                        scope: scope(None),
                        sort: SessionSort::StartTime,
                        order: SortOrder::Desc,
                        limit: 10,
                        cursor: None,
                        timeout,
                    },
                )
                .await;
            }
            ViewName::Scores => {
                let _ = validate_filter(&ctx, ViewName::Scores, text);
            }
            ViewName::ExperimentItems | ViewName::DatasetItems => {}
        }
    });
}

/// The data file the read cache holds in these entries: its last segment is the cache name.
const CACHE_OBJECT: &str =
    "tables/spans/project=p/day=2026-11-11/0-0192e5a0-7c1a-7000-8000-000000000001.parquet";
const CACHE_NAME: &str = "0-0192e5a0-7c1a-7000-8000-000000000001.parquet";

/// How the `size` file of a cached object is written.
#[derive(Debug, Arbitrary)]
enum SizeFile {
    Missing,
    True,
    Text(Vec<u8>),
}

/// How one block file of a cached object is written, around its valid bytes.
#[derive(Debug, Arbitrary)]
enum BlockFile {
    Missing,
    Good,
    BadCrc,
    Truncated(u16),
    Extended(Vec<u8>),
    Raw(Vec<u8>),
}

/// One cached object and the reads to make of it (track R M3).
#[derive(Debug, Arbitrary)]
struct CacheInput {
    /// Object length: `1 + len % (BLOCK + BLOCK / 2)`, so at most two blocks.
    len: u32,
    size_file: SizeFile,
    /// The cache files of the object's first two blocks.
    blocks: [BlockFile; 2],
    /// At most four reads: start `start % (len + 16)`, end `start + 1 + width % (BLOCK + 8)`.
    reads: Vec<(u32, u32)>,
}

/// The object's bytes: byte `i` is `i % 251`, as `read_cache.rs`'s tests.
fn cache_content(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

/// Block `index` of `content` as the cache writes it: its data, then the CRC-32C, little-endian.
fn good_block(content: &[u8], index: usize) -> Vec<u8> {
    let block = usize::try_from(BLOCK).unwrap_or(usize::MAX);
    let start = index.saturating_mul(block).min(content.len());
    let end = start.saturating_add(block).min(content.len());
    let data = &content[start..end];
    let mut out = data.to_vec();
    out.extend_from_slice(&crc32c::crc32c(data).to_le_bytes());
    out
}

/// Writes the cache directory of one object: its `size` file and the block files `input` describes.
fn write_cache_files(cache: &Path, input: &CacheInput, len: u64) {
    let object = cache.join(CACHE_NAME);
    std::fs::create_dir_all(&object).expect("create the cached object's directory");
    match &input.size_file {
        SizeFile::Missing => {}
        SizeFile::True => {
            std::fs::write(object.join("size"), len.to_string()).expect("write the size file");
        }
        SizeFile::Text(bytes) => {
            std::fs::write(object.join("size"), bytes).expect("write the size file");
        }
    }
    let content = cache_content(len);
    for (index, file) in input.blocks.iter().enumerate() {
        if u64::try_from(index).unwrap_or(u64::MAX) >= len.div_ceil(BLOCK) {
            continue;
        }
        let mut good = good_block(&content, index);
        let bytes = match file {
            BlockFile::Missing => continue,
            BlockFile::Good => good,
            BlockFile::BadCrc => {
                // The CRC's first byte.
                let at = good.len() - 4;
                good[at] ^= 0xff;
                good
            }
            BlockFile::Truncated(keep) => {
                let keep = usize::from(*keep) % good.len();
                good.truncate(keep);
                good
            }
            BlockFile::Extended(extra) => {
                good.extend_from_slice(extra);
                good
            }
            BlockFile::Raw(bytes) => bytes.clone(),
        };
        std::fs::write(object.join(index.to_string()), bytes).expect("write a block file");
    }
}

/// Reads `input`'s objects through a read cache, then again after the cache is reopened. A read
/// the cache can trust (its size is unknown or the true length) must return the true bytes.
fn check_cache(input: &CacheInput) {
    let len = 1 + u64::from(input.len) % (BLOCK + BLOCK / 2);
    let scratch = Scratch::new();
    let cache_dir = scratch.0.join("cache");
    write_cache_files(&cache_dir, input, len);

    let object = ObjectPath::from(CACHE_OBJECT);
    let content = Bytes::from(cache_content(len));
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    runtime()
        .block_on(inner.put(&object, PutPayload::from(content.clone())))
        // Harness setup, not input-dependent.
        .expect("store the cached object");

    for _round in 0..2 {
        let disk = Arc::new(Disk::open(vec![scratch.0.clone()], 0)
                // Harness setup, not input-dependent.
                .expect("open the disk"));
        let cache = Arc::new(
            ReadCache::open(cache_dir.clone(), 64 << 20, disk, 0)
                // Harness setup, not input-dependent.
                .expect("open the read cache"),
        );
        let store = CacheStore {
            inner: Arc::clone(&inner),
            cache: Arc::clone(&cache),
        };
        runtime().block_on(async {
            for (start, width) in input.reads.iter().take(4) {
                let start = u64::from(*start) % (len + 16);
                let end = start + 1 + u64::from(*width) % (BLOCK + 8);
                let trusted = cache.size(CACHE_NAME).is_none_or(|size| size == len);
                let result = store
                    .get_opts(
                        &object,
                        GetOptions {
                            range: Some(GetRange::Bounded(start..end)),
                            ..GetOptions::default()
                        },
                    )
                    .await;
                if start < len && trusted {
                    let bytes = result
                        .expect("a read of a trusted object succeeds")
                        .bytes()
                        .await
                        .expect("the bytes of a trusted read");
                    assert_eq!(
                        bytes,
                        content.slice(start as usize..end.min(len) as usize),
                        "a read returned bytes other than the object's"
                    );
                }
            }
        });
    }
}

/// Decodes arbitrary bytes as a read cache over one object, and reads it (track R M3).
pub fn read_cache_block(data: &[u8]) {
    quiet_caught_panics();
    let Ok(input) = CacheInput::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    check_cache(&input);
}

/// A text column of a catalog row. Each variant renders a value the loaders accept.
#[derive(Debug, Arbitrary)]
enum Text {
    Id(u8),
    Project(u8),
    Day(u16),
    Table(u8),
    State(u8),
    Kind(u8),
    Status(u8),
    Raw(String),
}

impl Text {
    fn render(&self) -> String {
        match self {
            Self::Id(n) => {
                Uuid::from_u128(0x0192_e5a0_7c1a_7000_8000_0000_0000_0000 | u128::from(*n))
                    .to_string()
            }
            Self::Project(n) => format!("018f6b4e-7c1a-7c1a-8c1a-0000000000{n:02x}"),
            Self::Day(n) => seed_day()
                .checked_add_days(chrono::Days::new(u64::from(*n % 400)))
                .unwrap_or_else(seed_day)
                .format("%Y-%m-%d")
                .to_string(),
            Self::Table(n) => ["spans", "traces", "scores", "logs"][usize::from(*n % 4)].to_owned(),
            Self::State(n) => ["active", "obsolete", "quarantined"][usize::from(*n % 3)].to_owned(),
            Self::Kind(n) => ["traces", "user", "other"][usize::from(*n % 3)].to_owned(),
            Self::Status(n) => ["pending", "applied"][usize::from(*n % 2)].to_owned(),
            Self::Raw(text) => text.clone(),
        }
    }
}

/// A `pack_id` blob for catalog fuzzing: either a valid 16-byte UUIDv7 or raw bytes.
#[derive(Debug, Arbitrary)]
enum PackIdBlob {
    Valid(u8),
    Raw(Vec<u8>),
}

impl PackIdBlob {
    fn render(&self) -> Vec<u8> {
        match self {
            Self::Valid(n) => {
                Uuid::from_u128(0x0192_e5a0_7c1a_7000_8000_0000_0000_0000 | u128::from(*n))
                    .as_bytes()
                    .to_vec()
            }
            Self::Raw(bytes) => bytes.clone(),
        }
    }
}

/// One `data_files` row.
#[derive(Debug, Arbitrary)]
struct FileRow {
    id: Text,
    table: Text,
    project: Text,
    day: Text,
    level: i64,
    path: Text,
    size: i64,
    footer: i64,
    rows: i64,
    min_time: i64,
    max_time: i64,
    min_ingest: i64,
    max_ingest: i64,
    fully_merged: i64,
    state: Text,
    applied_seq: i64,
    pack_id: Option<PackIdBlob>,
    pack_path: Option<Text>,
    pack_size: Option<i64>,
    pack_toc_size: Option<i64>,
}

/// One pending or applied deletion, with its scope rows.
#[derive(Debug, Arbitrary)]
struct DeletionRow {
    id: Text,
    seq: i64,
    project: Text,
    kind: Text,
    cutoff: i64,
    status: Text,
    registered_at: i64,
    traces: Vec<Vec<u8>>,
    users: Vec<String>,
    sessions: Vec<String>,
}

/// Catalog rows, written as they are (track R M3). Every list is cut to eight entries.
#[derive(Debug, Arbitrary)]
struct CatalogInput {
    files: Vec<FileRow>,
    sequences: Vec<(Text, i64)>,
    /// Indexes into `files`: (traces file, spans file).
    sources: Vec<(u8, u8)>,
    deletions: Vec<DeletionRow>,
    watermarks: Vec<(Text, Text)>,
    dropped: Vec<Text>,
    now: i64,
}

/// Inserts `input`'s rows. A row the schema or its constraints reject is skipped.
async fn insert_rows(connection: &mut SqliteConnection, input: &CatalogInput) {
    let files: Vec<&FileRow> = input.files.iter().take(8).collect();
    for row in &files {
        let _ = sqlx::query(
            "INSERT INTO data_files (id, table_name, project_id, day, level, path, size_bytes, \
             footer_size, row_count, min_time, max_time, min_ingest_time, max_ingest_time, \
             fully_merged, state, created_at, deletions_applied_seq, \
             pack_id, pack_path, pack_size_bytes, pack_toc_size) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?)",
        )
        .bind(row.id.render())
        .bind(row.table.render())
        .bind(row.project.render())
        .bind(row.day.render())
        .bind(row.level)
        .bind(row.path.render())
        .bind(row.size)
        .bind(row.footer)
        .bind(row.rows)
        .bind(row.min_time)
        .bind(row.max_time)
        .bind(row.min_ingest)
        .bind(row.max_ingest)
        .bind(row.fully_merged)
        .bind(row.state.render())
        .bind(row.applied_seq)
        .bind(row.pack_id.as_ref().map(PackIdBlob::render))
        .bind(row.pack_path.as_ref().map(Text::render))
        .bind(row.pack_size)
        .bind(row.pack_toc_size)
        .execute(&mut *connection)
        .await;
    }
    for (table, flushed) in input.sequences.iter().take(8) {
        let _ = sqlx::query("INSERT INTO table_state (table_name, flushed_seq) VALUES (?, ?)")
            .bind(table.render())
            .bind(flushed)
            .execute(&mut *connection)
            .await;
    }
    if !files.is_empty() {
        for (traces, spans) in input.sources.iter().take(8) {
            let traces = files[usize::from(*traces) % files.len()].id.render();
            let spans = files[usize::from(*spans) % files.len()].id.render();
            let _ = sqlx::query(
                "INSERT INTO file_sources (traces_file_id, spans_file_id) VALUES (?, ?)",
            )
            .bind(traces)
            .bind(spans)
            .execute(&mut *connection)
            .await;
        }
    }
    for deletion in input.deletions.iter().take(8) {
        let id = deletion.id.render();
        let _ = sqlx::query(
            "INSERT INTO deletions (id, deletion_seq, project_id, kind, cutoff, status, \
             registered_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(deletion.seq)
        .bind(deletion.project.render())
        .bind(deletion.kind.render())
        .bind(deletion.cutoff)
        .bind(deletion.status.render())
        .bind(deletion.registered_at)
        .execute(&mut *connection)
        .await;
        for trace in deletion.traces.iter().take(8) {
            let _ =
                sqlx::query("INSERT INTO deletion_traces (deletion_id, trace_id) VALUES (?, ?)")
                    .bind(&id)
                    .bind(trace)
                    .execute(&mut *connection)
                    .await;
        }
        for user in deletion.users.iter().take(8) {
            let _ = sqlx::query("INSERT INTO deletion_users (deletion_id, user_id) VALUES (?, ?)")
                .bind(&id)
                .bind(user)
                .execute(&mut *connection)
                .await;
        }
        for session in deletion.sessions.iter().take(8) {
            let _ = sqlx::query(
                "INSERT INTO deletion_sessions (deletion_id, session_id) VALUES (?, ?)",
            )
            .bind(&id)
            .bind(session)
            .execute(&mut *connection)
            .await;
        }
    }
    for (project, keep_from) in input.watermarks.iter().take(8) {
        let _ =
            sqlx::query("INSERT INTO retention_watermarks (project_id, keep_from) VALUES (?, ?)")
                .bind(project.render())
                .bind(keep_from.render())
                .execute(&mut *connection)
                .await;
    }
    for project in input.dropped.iter().take(8) {
        let _ = sqlx::query(
            "INSERT INTO projects (project_id, state, dropped_at) VALUES (?, 'dropped', 0)",
        )
        .bind(project.render())
        .execute(&mut *connection)
        .await;
    }
}

/// Writes `input`'s rows into an in-memory catalog and runs the loads `Committer::open` and
/// `Storage::open` run. `None` when a load rejects the rows.
fn open_catalog(input: &CatalogInput) -> Option<Catalog> {
    runtime().block_on(async {
        let mut connection = SqliteConnectOptions::from_str("sqlite::memory:")
            // Harness setup, not input-dependent.
            .expect("the in-memory catalog URL parses")
            .foreign_keys(true)
            .connect()
            .await
            // Harness setup, not input-dependent.
            .expect("open an in-memory catalog");
        sqlx::migrate!("./migrations/catalog")
            .run(&mut connection)
            .await
            // Harness setup, not input-dependent.
            .expect("migrate the in-memory catalog");
        insert_rows(&mut connection, input).await;
        let (catalog, _) = load(&mut connection).await.ok()?;
        check_file_sources(&mut connection).await.ok()?;
        load_tombstones(&mut connection).await.ok()?;
        load_project_states(&mut connection).await.ok()?;
        Some(catalog)
    })
}

/// Decodes arbitrary bytes as catalog rows, loads them, and plans compaction over the result
/// (track R M3).
pub fn catalog_rows(data: &[u8]) {
    quiet_caught_panics();
    let Ok(input) = CatalogInput::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    let Some(catalog) = open_catalog(&input) else {
        return;
    };
    let policy = Policy::from(&CompactionConfig::default());
    let _ = plan(&catalog, &HashSet::new(), &policy, input.now);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use cetra_column::Batch;
    use chrono::DateTime;

    use super::*;
    use crate::file_format::{WrittenFile, write_file_batch};
    use crate::test_support::{dataset_scores, dataset_spans, score_rows, span_rows, trace_batch};
    use crate::wal::{HEADER_LEN, sample_span_batch};

    /// Filter seeds: the view byte, then the filter text (`filter/mod.rs`'s tests).
    const FILTER_SEEDS: [(u8, &str); 8] = [
        (1, "status = 'error'"),
        (1, "cost_total > 0.001 AND tags @> ['beta']"),
        (0, "start_time >= '2026-01-01T10:05:00Z'"),
        (0, "start_time > now() - INTERVAL '3650 days'"),
        (2, "trace_count >= 2"),
        (3, "name = 'helpfulness' AND value_num > 0.5"),
        (0, "environment = 'production' OR status = 'ok'"),
        (1, "name = 'a' extra"),
    ];

    /// The first byte of a Parquet entry input, which selects the table.
    fn table_byte(table: FileTable) -> u8 {
        match table {
            FileTable::Spans => 0,
            FileTable::Traces => 1,
            FileTable::Scores => 2,
        }
    }

    /// One written Parquet file of `table`, from the dataset.
    pub(crate) fn parquet_bytes(table: FileTable) -> WrittenFile {
        let batch: Batch = match table {
            FileTable::Spans => span_rows(&dataset_spans()),
            FileTable::Traces => trace_batch(&dataset_spans()),
            FileTable::Scores => score_rows(&dataset_scores()),
        };
        write_file_batch(table, 0, &[Arc::new(batch)]).expect("write a test file")
    }

    /// The entry input for `parquet_file`: the table byte, the footer hint, a zero bound, the file.
    pub(crate) fn seed_parquet(table: FileTable) -> Vec<u8> {
        let written = parquet_bytes(table);
        let mut out = vec![table_byte(table)];
        out.extend_from_slice(
            &u32::try_from(written.footer_size)
                .expect("a footer fits in u32")
                .to_le_bytes(),
        );
        out.extend_from_slice(&0_i64.to_le_bytes());
        out.extend_from_slice(&written.bytes);
        out
    }

    /// Stores `written` at the entry's fixed path and returns its read state.
    fn stored_query(table: FileTable) -> (QueryContext, Arc<VerifyQueue>, FileMeta) {
        let written = parquet_bytes(table);
        let meta = parquet_meta(table, written.bytes.len() as u64, written.footer_size);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        runtime().block_on(async {
            store
                .put(
                    &ObjectPath::from(meta.path.as_str()),
                    PutPayload::from(written.bytes.clone()),
                )
                .await
                .expect("store the test file")
        });
        let (ctx, verify) = parquet_query(store, &meta);
        (ctx, verify, meta)
    }

    #[test]
    fn dataset_falls_on_the_seed_day() {
        for span in dataset_spans() {
            let day = DateTime::from_timestamp_nanos(span.start_time).date_naive();
            assert_eq!(day, seed_day());
        }
        for score in dataset_scores() {
            let day = DateTime::from_timestamp_nanos(score.created_at).date_naive();
            assert_eq!(day, seed_day());
        }
    }

    #[test]
    fn parquet_file_corpus_replays() {
        replay("parquet_file", parquet_file);
    }

    #[test]
    fn filter_dsl_corpus_replays() {
        replay("filter_dsl", filter_dsl);
    }

    #[test]
    fn parquet_file_reads_a_seed() {
        let (ctx, _, _) = stored_query(FileTable::Spans);
        let request = SpanListRequest {
            strict_exclusions: false,
            filter: None,
            search: None,
            scope: scope(None),
            sort: SpanSort::StartTime,
            order: SortOrder::Desc,
            limit: 100,
            cursor: None,
            timeout: Duration::from_secs(30),
        };
        let page = runtime()
            .block_on(span_list(&ctx, request))
            .expect("the seed file reads");
        let rows: usize = page.batches.iter().map(Batch::num_rows).sum();
        assert_eq!(rows, dataset_spans().len());
    }

    /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
    /// `fuzz/regressions/<target>/`. The corpus must not be empty.
    fn replay(target: &str, entry: fn(&[u8])) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus").join(target));
        assert!(!corpus.is_empty(), "no corpus for {target}");
        let regressions = sorted_files(&root.join("regressions").join(target));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read a corpus file");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&data)));
            if outcome.is_err() {
                panic!("{target}: {} panicked", path.display());
            }
        }
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|entry| entry.expect("read a corpus entry").path())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        files
    }

    /// The fuzz corpus directory for one target.
    fn corpus_dir(target: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus")
            .join(target)
    }

    /// The framed batch payload of one batch of `table`, without its outer record header.
    fn ipc_payload(table: Table) -> Vec<u8> {
        let batch: Batch = match table {
            Table::Spans => sample_span_batch(),
            Table::Scores => score_rows(&dataset_scores()),
        };
        let mut out = Vec::new();
        encode(1, &batch, &mut out).expect("encode a test batch");
        out[HEADER_LEN..].to_vec()
    }

    /// The entry input for `wal_replay`: one framed segment per `(first_seq, record seqs, tear)`.
    fn seed_wal_replay(
        table: Table,
        skip_corrupt: bool,
        segments: &[(u64, &[u64], u8)],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let table_bit = u8::from(table == Table::Scores);
        out.push(table_bit | (u8::from(skip_corrupt) << 1));
        out.push(u8::try_from(segments.len() - 1).expect("at most three segments"));
        out.extend_from_slice(&0_u64.to_le_bytes());
        for (first_seq, seqs, tear) in segments {
            out.extend_from_slice(&first_seq.to_le_bytes());
            out.push(tear << 1);
            out.push(u8::try_from(seqs.len()).expect("at most four frames"));
            for seq in *seqs {
                let payload = ipc_payload(table);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(
                    &u32::try_from(payload.len())
                        .expect("small payload")
                        .to_le_bytes(),
                );
                out.extend_from_slice(&payload);
            }
        }
        out
    }

    /// Runs the `wal_replay` steps once by hand and returns the record count.
    fn first_replay_records(data: &[u8]) -> u64 {
        let scratch = Scratch::new();
        let staged = stage(data, &scratch);
        let first = recover_table(
            &staged.dir,
            staged.table,
            staged.after_seq,
            staged.skip_corrupt,
            WalConfig::default().segment_bytes.0,
            &Hooks::default(),
        )
        .expect("a seeded WAL replays");
        first.records
    }

    #[test]
    fn wal_record_corpus_replays() {
        replay("wal_record", wal_record);
        let reg_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/regressions/wal_record");
        for path in sorted_files(&reg_dir) {
            let data = std::fs::read(&path).expect("read a regression file");
            for table in [Table::Spans, Table::Scores] {
                assert!(
                    matches!(
                        read_batch(&data, table),
                        Err(crate::error::WalError::InvalidPayload { .. })
                    ),
                    "expected InvalidPayload for legacy regression {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn wal_replay_corpus_replays() {
        replay("wal_replay", wal_replay);
    }

    #[test]
    fn wal_replay_replays_framed_records() {
        let data = seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 0)]);
        assert_eq!(first_replay_records(&data), 3);
    }

    #[test]
    fn wal_replay_truncates_a_torn_frame() {
        let data = seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 5)]);
        assert_eq!(first_replay_records(&data), 2);
        // The entry's second replay must agree with the first; the entry asserts it.
        wal_replay(&data);
    }

    #[test]
    fn read_cache_block_corpus_replays() {
        replay("read_cache_block", read_cache_block);
    }

    #[test]
    fn catalog_rows_corpus_replays() {
        replay("catalog_rows", catalog_rows);
    }

    /// A 1.5-block object whose first block fails its CRC and whose second is cut short: every
    /// read across the boundary must still return the true bytes, from the store.
    #[test]
    fn read_cache_serves_true_bytes_over_bad_blocks() {
        let input = CacheInput {
            len: 1_572_863,
            size_file: SizeFile::True,
            blocks: [BlockFile::BadCrc, BlockFile::Truncated(10)],
            reads: vec![((1 << 20) - 10, 21), (0, 100)],
        };
        check_cache(&input);
    }

    /// Catalog rows built from the valid text variants load, and a file's traces source is found.
    #[test]
    fn catalog_rows_loads_a_consistent_catalog() {
        let file = |id: u8, table: u8| FileRow {
            id: Text::Id(id),
            table: Text::Table(table),
            project: Text::Project(1),
            day: Text::Day(0),
            level: 0,
            path: Text::Raw(format!("tables/x/{id}.parquet")),
            size: 100,
            footer: 10,
            rows: 5,
            min_time: 0,
            max_time: 10,
            min_ingest: 0,
            max_ingest: 10,
            fully_merged: 0,
            state: Text::State(0),
            applied_seq: 0,
            pack_id: None,
            pack_path: None,
            pack_size: None,
            pack_toc_size: None,
        };
        let input = CatalogInput {
            // The spans file (index 0) and its traces file (index 1).
            files: vec![file(1, 0), file(2, 1)],
            sequences: Vec::new(),
            sources: vec![(1, 0)],
            deletions: Vec::new(),
            watermarks: Vec::new(),
            dropped: Vec::new(),
            now: 0,
        };
        let catalog = open_catalog(&input).expect("a consistent catalog loads");
        assert_eq!(catalog.files_all(FileTable::Spans).count(), 1);
        assert_eq!(catalog.files_all(FileTable::Traces).count(), 1);
        let policy = Policy::from(&CompactionConfig::default());
        let _ = plan(&catalog, &HashSet::new(), &policy, 0);
    }

    /// Writes the seed files of this crate's targets (track R M3, `just fuzz-seed`).
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let write = |target: &str, name: &str, bytes: &[u8]| {
            let dir = corpus_dir(target);
            std::fs::create_dir_all(&dir).expect("create the corpus directory");
            std::fs::write(dir.join(format!("seed-{name}")), bytes).expect("write a seed");
        };
        write("wal_record", "spans", &ipc_payload(Table::Spans));
        write("wal_record", "scores", &ipc_payload(Table::Scores));
        write(
            "wal_replay",
            "one-segment",
            &seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 0)]),
        );
        write(
            "wal_replay",
            "torn-tail",
            &seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 5)]),
        );
        write(
            "wal_replay",
            "two-segments",
            &seed_wal_replay(Table::Scores, true, &[(1, &[1, 2], 0), (3, &[3], 0)]),
        );
        for (table, name) in [
            (FileTable::Spans, "spans"),
            (FileTable::Traces, "traces"),
            (FileTable::Scores, "scores"),
        ] {
            write("parquet_file", name, &seed_parquet(table));
        }
        for (index, (view, text)) in FILTER_SEEDS.iter().enumerate() {
            let mut bytes = vec![*view];
            bytes.extend_from_slice(text.as_bytes());
            write("filter_dsl", &format!("{:02}", index + 1), &bytes);
        }
        // `arbitrary` decodes any bytes into an input, so the seeds only start the fuzzer.
        for target in ["read_cache_block", "catalog_rows"] {
            write(target, "zeros-64", &[0_u8; 64]);
            write(target, "ones-64", &[0xff_u8; 64]);
            let ramp: Vec<u8> = (0..=255_u8).collect();
            write(target, "ramp-256", &ramp);
            let pattern: Vec<u8> = (0..1024_u32)
                .map(|i| u8::try_from((i * 31) % 256).unwrap_or(0))
                .collect();
            write(target, "pattern-1024", &pattern);
        }
    }
}
