//! Fuzz entry points (track R M3, docs/plans/r-hardening/03-m3-fuzzing.md). Each one takes arbitrary bytes and must not panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use chrono::NaiveDate;
use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use llmobs_core::ids::ProjectId;
use llmobs_core::schema::{scores, spans};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use uuid::Uuid;

use crate::anchor::Anchors;
use crate::catalog::{Catalog, CatalogEdit, FileId, FileMeta, FileTable};
use crate::compaction::slice::time_values;
use crate::config::WalConfig;
use crate::filter::FilterView;
use crate::hooks::Hooks;
use crate::memtable::Memtable;
use crate::objects::OBJECT_STORE_URL;
use crate::read_state::ReadState;
use crate::verify::{VerifyQueue, verify_file};
use crate::wal::{Decoded, Table, decode_next, encode, push_framed, read_batch, segment_name};
use crate::{
    ListScope, QueryContext, QueryOptions, ScoreListRequest, SortOrder, SpanListRequest, SpanSort,
    TimeWindow, TraceListRequest, TraceSort, ViewName, project_query, recover_table, replay_batch,
    score_list, span_list, trace_detail, trace_list,
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
        let path = std::env::temp_dir().join(format!("llmobs-fuzz-{}", Uuid::now_v7()));
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

/// Frames arbitrary bytes as WAL records and decodes them; the IPC payload is read as a batch.
pub fn wal_record(data: &[u8]) {
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
            let mut segment = Vec::new();
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
    let scratch = Scratch::new();
    let staged = stage(data, &scratch);
    let segment_bytes = WalConfig::default().segment_bytes.0;
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

fn two() -> NonZeroUsize {
    // Harness setup, not input-dependent.
    NonZeroUsize::new(2).expect("2 is not zero")
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
        rows: 0,
        // Every row falls inside the file's range, so no pruning skips the file.
        min_time: i64::MIN,
        max_time: i64::MAX,
        min_ingest_time: i64::MIN,
        max_ingest_time: i64::MAX,
        fully_merged: false,
        deletions_applied_seq: 0,
    }
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
        remove: Vec::new(),
        flushed_seq: None,
        sources: Vec::new(),
        raise: Vec::new(),
        quarantine: Vec::new(),
    };
    let (catalog, _) = Catalog::empty()
        .apply(&edit)
        .expect("one new file applies to an empty catalog");
    let state = ReadState {
        catalog: Arc::new(catalog),
        ..ReadState::empty()
    };
    let runtime_env = RuntimeEnvBuilder::new()
        .build_arc()
        .expect("build the query runtime");
    let url = ObjectStoreUrl::parse(OBJECT_STORE_URL).expect("the object store URL parses");
    runtime_env.register_object_store(url.as_ref(), store);
    let verify = Arc::new(VerifyQueue::default());
    let query = project_query(
        &state,
        project(),
        QueryOptions::default(),
        &runtime_env,
        two(),
        DAY_NS,
        &verify,
    )
    .expect("a query over one file plans");
    let ctx = QueryContext::new(query, None).expect("the query context builds");
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

/// The view a filter view compiles against, by its table name.
fn view_table(view: FilterView) -> &'static str {
    match view {
        FilterView::Traces => ViewName::Traces,
        FilterView::Spans => ViewName::Spans,
        FilterView::Sessions => ViewName::Sessions,
        FilterView::Scores => ViewName::Scores,
    }
    .as_str()
}

/// Parses arbitrary text as a filter over one view and runs it (track R M3). Layout:
/// byte 0 selects the view, the rest must be UTF-8.
pub fn filter_dsl(data: &[u8]) {
    let Some((&view, text)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    let view = match view % 4 {
        0 => FilterView::Traces,
        1 => FilterView::Spans,
        2 => FilterView::Sessions,
        _ => FilterView::Scores,
    };
    let text = text.to_owned();
    // A tokio worker's stack size: the server parses filters on one (`filter/mod.rs` tests it).
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
fn run_filter(view: FilterView, text: &str) {
    let verify = Arc::new(VerifyQueue::default());
    let runtime_env = RuntimeEnvBuilder::new()
        .build_arc()
        // Harness setup, not input-dependent.
        .expect("build the query runtime");
    let query = project_query(
        &ReadState::empty(),
        project(),
        QueryOptions::default(),
        &runtime_env,
        two(),
        DAY_NS,
        &verify,
    )
    // Harness setup, not input-dependent.
    .expect("a query over empty tables plans");
    let ctx = QueryContext::new(query, None)
        // Harness setup, not input-dependent.
        .expect("the query context builds");
    let Ok(filter) = ctx.parse_filter(view, text) else {
        return;
    };
    runtime().block_on(async {
        let Ok(frame) = ctx.session().table(view_table(view)).await else {
            return;
        };
        let Ok(frame) = filter.apply(frame, &ctx).await else {
            return;
        };
        let _ = frame.collect().await;
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use arrow::array::RecordBatch;
    use chrono::DateTime;

    use super::*;
    use crate::parquet::{WrittenFile, write_file};
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
        let batch: RecordBatch = match table {
            FileTable::Spans => span_rows(&dataset_spans()),
            FileTable::Traces => trace_batch(&dataset_spans()),
            FileTable::Scores => score_rows(&dataset_scores()),
        };
        write_file(table, 0, &[Arc::new(batch)]).expect("write a test file")
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
        let rows: usize = page.batches.iter().map(RecordBatch::num_rows).sum();
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

    /// The IPC payload of one batch of `table`, without its record header.
    fn ipc_payload(table: Table) -> Vec<u8> {
        let batch: RecordBatch = match table {
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
    }
}
