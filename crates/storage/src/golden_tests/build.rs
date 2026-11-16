//! The golden scenario: lives that write a data directory (track R M8, D-R-29).
//!
//! Only public `Storage` functions and the public `reads` are called, the way the server calls
//! them. The read cache entry (step 7) is written through its crate-private API, as the plan says.
//! The values are fixed: `dump.rs` reads them back and `mod.rs` names the constants.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use bytesize::ByteSize;
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::score::{Score, ScoreTarget, ScoreValue};
use llmobs_core::span::Span;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

use super::dump::{block_on, catalog_rows};
use super::{CACHE_NAME, CACHE_SIZE, DELETION_TRACE, DELETION_USER, MEDIA, P1, P2, P3, ns_at};
use crate::catalog::FileTable;
use crate::config::{
    CompactionConfig, FlushConfig, GcConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig,
    WalConfig, WalSync,
};
use crate::deletion::{Deletion, DeletionScope};
use crate::disk::{Disk, test_spill};
use crate::read_cache::ReadCache;
use crate::test_support::{clean_life, crashed_life, score_row, score_rows, span_row, span_rows};
use crate::wal::Table;
use crate::{MediaBlob, QueryContext, QueryOptions, Storage, TimeWindow, user_deletion_scope};

/// How long each group's flush may take.
const FLUSH_WAIT: Duration = Duration::from_secs(30);

/// The compaction and the deletion pass run at this instant (2026-02-12T00:00:00Z) every time.
fn now() -> i64 {
    ns_at("2026-02-12", 0)
}

/// The cutoff of both deletions: at or after every `ingest_time` of the scenario.
fn cutoff() -> i64 {
    ns_at("2026-02-11", 0)
}

/// The config of every life of the scenario and of every dump of a copy of it. `min_free_bytes`
/// is 0, objects are local, and the timings let `compact(NOW)` compact: a day closes one second
/// after it ends, and the L0 age is one second. The GC grace is 30 days, so an open collects
/// nothing in the scenario.
pub(super) fn golden_config(dir: &Path) -> StorageConfig {
    StorageConfig {
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig {
            sync: WalSync::Always,
            ..WalConfig::default()
        },
        flush: FlushConfig {
            max_age: Duration::from_secs(3_600),
            ..FlushConfig::default()
        },
        memtable: MemtableConfig::default(),
        query: QueryConfig::default(),
        anchor: Default::default(),
        gc: GcConfig {
            grace: Duration::from_secs(30 * 24 * 3_600),
        },
        compaction: CompactionConfig {
            interval: Duration::from_secs(3_600),
            l0_files: 2,
            l0_age: Duration::from_secs(1),
            close_delay: Duration::from_secs(1),
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

/// Writes the golden scenario into `dir` (must not exist).
pub(super) fn build(dir: &Path) {
    assert!(
        !dir.exists(),
        "the golden directory must not exist before it is built"
    );
    let config = golden_config(dir);
    clean_life(&config, async |storage| ingest(storage).await);
    clean_life(&config, async |storage| maintain(storage).await);
    settle_catalog(dir);
    crashed_life(&config, async |storage| wal_tail(storage).await);
    settle_catalog(dir);
    assert_wal_tail(dir);
    write_cache_entry(dir);
    clean_tree(dir);
}

/// The ids and the trace numbers of the scenario. A trace number `n` is the trace id of
/// [`trace_id`]; spans are numbered within their trace.
fn project(text: &str) -> ProjectId {
    text.parse().expect("a golden project id")
}

fn trace_id(trace: u16) -> TraceId {
    let pair = (trace + 1).to_be_bytes();
    let mut bytes = [0_u8; 16];
    for index in 0..8 {
        bytes[2 * index..2 * index + 2].copy_from_slice(&pair);
    }
    TraceId::from_bytes(bytes).expect("a nonzero trace id")
}

fn span_id(number: u8) -> SpanId {
    SpanId::from_bytes([number + 1; 8]).expect("a nonzero span id")
}

const MS: i64 = 1_000_000;

/// One LLM call of a chat trace: its tokens, its dyadic cost, its status and an optional text.
#[derive(Clone, Copy)]
struct Call {
    input: i64,
    output: i64,
    cost: f64,
    status: SpanStatus,
    text: Option<(&'static str, &'static str)>,
}

/// The base of every span: the trace, its number, its parent, and its user and session.
#[allow(clippy::too_many_arguments)]
fn base(
    project: ProjectId,
    trace: u16,
    number: u8,
    parent: Option<u8>,
    name: &str,
    kind: SpanKind,
    start: i64,
    ingest: i64,
    user: &str,
    session: &str,
) -> Span {
    let mut span = span_row(trace, number, start, ingest);
    span.project_id = project;
    span.parent_span_id = parent.map(span_id);
    span.name = name.to_owned();
    span.span_kind = kind;
    span.user_id = Some(user.to_owned());
    span.session_id = Some(session.to_owned());
    span
}

/// A chat trace: a root (`chat`), an LLM span (`llm`) and, if `tool`, a tool span (`lookup`).
#[allow(clippy::too_many_arguments)]
fn chat(
    project: ProjectId,
    trace: u16,
    start: i64,
    ingest: i64,
    user: &str,
    session: &str,
    call: Call,
    tool: bool,
) -> Vec<Span> {
    let mut root = base(
        project,
        trace,
        0,
        None,
        "chat",
        SpanKind::Chain,
        start,
        ingest,
        user,
        session,
    );
    root.end_time = start + 3 * MS;
    root.tags = vec!["golden".to_owned()];
    root.environment = Some("golden".to_owned());

    let mut llm = base(
        project,
        trace,
        1,
        Some(0),
        "llm",
        SpanKind::Llm,
        start + MS,
        ingest,
        user,
        session,
    );
    llm.end_time = start + 2 * MS;
    llm.request_model = Some("model-a".to_owned());
    llm.response_model = Some("model-a".to_owned());
    llm.provider = Some("golden-provider".to_owned());
    llm.operation = Some("chat".to_owned());
    llm.input_tokens = Some(call.input);
    llm.output_tokens = Some(call.output);
    llm.total_tokens = Some(call.input + call.output);
    llm.cost_total = Some(call.cost);
    llm.status = call.status;
    if call.status == SpanStatus::Error {
        llm.status_message = Some("rate limited".to_owned());
    }
    if let Some((input, output)) = call.text {
        llm.input = Some(input.to_owned());
        llm.output = Some(output.to_owned());
        llm.attributes
            .insert("llmobs.golden".to_owned(), "1".to_owned());
    }

    let mut spans = vec![root, llm];
    if tool {
        let mut lookup = base(
            project,
            trace,
            2,
            Some(0),
            "lookup",
            SpanKind::Tool,
            start + 2 * MS,
            ingest,
            user,
            session,
        );
        lookup.end_time = start + 3 * MS;
        spans.push(lookup);
    }
    spans
}

async fn append_spans(storage: &Storage, project: ProjectId, spans: &[Span]) {
    storage
        .append_spans(project, span_rows(spans))
        .await
        .expect("append a golden group of spans");
}

async fn append_scores(storage: &Storage, project: ProjectId, scores: &[Score]) {
    storage
        .append_scores(project, score_rows(scores))
        .await
        .expect("append a golden group of scores");
}

async fn flush(storage: &Storage) {
    storage
        .flush_now(FLUSH_WAIT)
        .await
        .expect("flush a golden group");
}

/// The `ingest_time` of a row: day D1 at `seconds` after midnight.
fn ingest_at(seconds: i64) -> i64 {
    ns_at("2026-02-10", seconds)
}

/// A score of `project` on trace `trace`, created and updated at `created` (day D1 or D2 seconds).
fn trace_score(
    project: ProjectId,
    id: u128,
    trace: u16,
    created: i64,
    ingest: i64,
    name: &str,
    value: ScoreValue,
) -> Score {
    let mut score = score_row(id, trace, created, created, ingest, false, name);
    score.project_id = project;
    score.value = value;
    score
}

/// Life 1: spans, scores and a media blob, in groups, each flushed (so each group is an L0 file).
async fn ingest(storage: &Storage) {
    let p1 = project(P1);
    let p2 = project(P2);
    let p3 = project(P3);

    // Group A: P1 day D1, traces 1 and 2.
    let mut group = chat(
        p1,
        1,
        ns_at("2026-02-10", 100),
        ingest_at(1),
        "u-1",
        "s-1",
        Call {
            input: 10,
            output: 5,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: Some(("question one", "answer one")),
        },
        true,
    );
    group.extend(chat(
        p1,
        2,
        ns_at("2026-02-10", 200),
        ingest_at(2),
        "u-2",
        "s-2",
        Call {
            input: 20,
            output: 10,
            cost: 0.5,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    ));
    append_spans(storage, p1, &group).await;
    flush(storage).await;

    // Group B: P1 day D1, trace 3 with an error.
    let group = chat(
        p1,
        3,
        ns_at("2026-02-10", 300),
        ingest_at(3),
        "u-1",
        "s-1",
        Call {
            input: 7,
            output: 3,
            cost: 0.25,
            status: SpanStatus::Error,
            text: None,
        },
        true,
    );
    append_spans(storage, p1, &group).await;
    flush(storage).await;

    // Group C: P1 day D2 (traces 4 and 5), P2 days D1 and D0, P3 day D1 (dropped later).
    let mut p1_group = chat(
        p1,
        4,
        ns_at("2026-02-11", 100),
        ingest_at(4),
        "u-2",
        "s-2",
        Call {
            input: 12,
            output: 6,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    );
    p1_group.extend(chat(
        p1,
        5,
        ns_at("2026-02-11", 200),
        ingest_at(5),
        "u-1",
        "s-1",
        Call {
            input: 8,
            output: 4,
            cost: 0.5,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    ));
    append_spans(storage, p1, &p1_group).await;

    let mut p2_group = chat(
        p2,
        6,
        ns_at("2026-02-10", 100),
        ingest_at(6),
        "u-3",
        "s-3",
        Call {
            input: 5,
            output: 5,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    );
    p2_group.extend(chat(
        p2,
        7,
        ns_at("2026-02-10", 200),
        ingest_at(7),
        "u-3",
        "s-3",
        Call {
            input: 4,
            output: 2,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    ));
    p2_group.extend(chat(
        p2,
        8,
        ns_at("2026-02-01", 100),
        ingest_at(8),
        "u-3",
        "s-3",
        Call {
            input: 3,
            output: 1,
            cost: 0.5,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    ));
    append_spans(storage, p2, &p2_group).await;

    let p3_group = chat(
        p3,
        9,
        ns_at("2026-02-10", 100),
        ingest_at(9),
        "u-4",
        "s-4",
        Call {
            input: 3,
            output: 1,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        true,
    );
    append_spans(storage, p3, &p3_group).await;
    flush(storage).await;

    // Group S: scores. P1 day D1 (traces 1 and 2, a span of trace 3): a numeric score with a later
    // update, a boolean, a categorical with a comment, and a span score that is then deleted.
    const QUALITY: u128 = 0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e1;
    const PASSED: u128 = 0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e2;
    const LABEL: u128 = 0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e3;
    const SPAN_QUALITY: u128 = 0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e4;
    const P2_QUALITY: u128 = 0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e5;
    let d1 = |seconds| ns_at("2026-02-10", seconds);

    let quality_first = trace_score(
        p1,
        QUALITY,
        1,
        d1(500),
        ingest_at(20),
        "quality",
        ScoreValue::Numeric(0.5),
    );
    let quality_update = trace_score(
        p1,
        QUALITY,
        1,
        d1(600),
        ingest_at(21),
        "quality",
        ScoreValue::Numeric(0.75),
    );
    let passed = trace_score(
        p1,
        PASSED,
        2,
        d1(510),
        ingest_at(22),
        "passed",
        ScoreValue::Boolean(true),
    );
    let mut label = trace_score(
        p1,
        LABEL,
        1,
        d1(520),
        ingest_at(23),
        "label",
        ScoreValue::Categorical {
            label: "good".to_owned(),
            value: Some(1.0),
        },
    );
    label.reason = Some("matches the reference".to_owned());
    let span_target = ScoreTarget::Span {
        trace_id: trace_id(3),
        span_id: span_id(1),
    };
    let mut span_score = trace_score(
        p1,
        SPAN_QUALITY,
        3,
        d1(530),
        ingest_at(24),
        "quality",
        ScoreValue::Numeric(0.25),
    );
    span_score.target = span_target.clone();
    let mut span_deleted = score_row(
        SPAN_QUALITY,
        3,
        d1(540),
        d1(540),
        ingest_at(25),
        true,
        "quality",
    );
    span_deleted.project_id = p1;
    span_deleted.target = span_target;
    span_deleted.value = ScoreValue::Numeric(0.25);
    append_scores(
        storage,
        p1,
        &[
            quality_first,
            quality_update,
            passed,
            label,
            span_score,
            span_deleted,
        ],
    )
    .await;

    let p2_score = trace_score(
        p2,
        P2_QUALITY,
        6,
        d1(500),
        ingest_at(26),
        "quality",
        ScoreValue::Numeric(0.25),
    );
    append_scores(storage, p2, &[p2_score]).await;
    flush(storage).await;

    // The media blob of P1, referenced by its trace 1 (newest span start).
    let digest: [u8; 32] = Sha256::digest(MEDIA).into();
    storage
        .put_media(
            p1,
            MediaBlob {
                sha256: digest,
                mime_type: "image/png".to_owned(),
                bytes: Bytes::from_static(MEDIA),
                newest_span_start: ns_at("2026-02-10", 100),
            },
        )
        .await
        .expect("put the golden blob");
}

/// Life 2: compaction until it is empty, the replaced files collected, deletions, retention, drop.
async fn maintain(storage: &Storage) {
    let mut passes = 0;
    loop {
        let report = storage.compact(now()).await.expect("a compaction pass");
        if report.jobs == 0 && report.failed == 0 {
            break;
        }
        passes += 1;
        assert!(
            passes < 10,
            "compaction of the golden scenario does not settle"
        );
    }
    assert!(
        storage
            .catalog_files(FileTable::Spans)
            .iter()
            .any(|file| file.level == 1),
        "compaction makes an L1 spans file of the golden scenario"
    );
    storage
        .collect_obsolete(Duration::ZERO)
        .await
        .expect("collect the replaced files");

    let p1 = project(P1);
    // A trace deletion of trace 3, applied.
    storage
        .register_deletion(Deletion {
            id: DELETION_TRACE,
            project: p1,
            cutoff: cutoff(),
            scope: DeletionScope::Traces {
                trace_ids: vec![trace_id(3)],
            },
        })
        .await
        .expect("register the trace deletion");
    let applied = storage
        .apply_deletions(now())
        .await
        .expect("apply the deletions");
    assert!(applied.applied >= 1, "the trace deletion is applied");

    // A user deletion of u-2, left pending: its scope from the public read.
    let temp_ctx = QueryContext::new(
        storage
            .query(p1, QueryOptions::new(None, None))
            .await
            .expect("query P1"),
        Some(TimeWindow::new(ns_at("2026-02-01", 0), ns_at("2026-02-12", 0)).expect("a window")),
    )
    .expect("a query context");
    let scope = user_deletion_scope(&temp_ctx, "u-2", cutoff(), Duration::from_secs(30))
        .await
        .expect("the user deletion scope of u-2");
    storage
        .register_deletion(Deletion {
            id: DELETION_USER,
            project: p1,
            cutoff: cutoff(),
            scope: DeletionScope::User {
                user_id: "u-2".to_owned(),
                trace_ids: scope.trace_ids,
                session_ids: scope.session_ids,
            },
        })
        .await
        .expect("register the user deletion");

    // Retention drops P2's day D0 (its files stay obsolete: no GC after this), and P3 is dropped.
    let keep_from = chrono::NaiveDate::from_ymd_opt(2026, 2, 10).expect("a day");
    storage
        .apply_retention(project(P2), keep_from)
        .await
        .expect("apply retention to P2");
    storage.drop_project(project(P3)).await.expect("drop P3");
}

/// Life 3, the WAL tail (D-R-4): one trace of P1 day D2 and two scores, never flushed.
async fn wal_tail(storage: &Storage) {
    let p1 = project(P1);
    let spans = chat(
        p1,
        10,
        ns_at("2026-02-11", 300),
        ingest_at(10),
        "u-1",
        "s-1",
        Call {
            input: 2,
            output: 2,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        false,
    );
    append_spans(storage, p1, &spans).await;
    let quality = trace_score(
        p1,
        0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e6,
        10,
        ns_at("2026-02-11", 400),
        ingest_at(27),
        "quality",
        ScoreValue::Numeric(0.9),
    );
    let label = trace_score(
        p1,
        0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00e7,
        10,
        ns_at("2026-02-11", 410),
        ingest_at(28),
        "label",
        ScoreValue::Categorical {
            label: "bad".to_owned(),
            value: None,
        },
    );
    append_scores(storage, p1, &[quality, label]).await;
}

/// The hex trace id of the scenario's trace number `trace` (as `trace_list` writes it).
pub(super) fn trace_hex(trace: u16) -> String {
    trace_id(trace).to_string()
}

/// The write probe of `check_dir` step 8 (R8.7): one trace of two spans in P1 starting at `start`,
/// with a user and a session of its own, built by the scenario's own `chat`.
pub(super) fn probe(start: i64) -> Vec<Span> {
    chat(
        project(P1),
        11,
        start,
        ingest_at(29),
        "u-probe",
        "s-probe",
        Call {
            input: 2,
            output: 1,
            cost: 0.25,
            status: SpanStatus::Ok,
            text: None,
        },
        false,
    )
}

/// Waits up to 5 s for the catalog's WAL and shared memory files to go (a closed connection
/// removes them); if one stays, checkpoints it through a connection of its own. Asserts that
/// both are gone.
fn settle_catalog(dir: &Path) {
    let wal = dir.join("catalog.db-wal");
    let shm = dir.join("catalog.db-shm");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (wal.exists() || shm.exists()) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if wal.exists() || shm.exists() {
        block_on(async {
            let options = SqliteConnectOptions::new().filename(dir.join("catalog.db"));
            let mut connection = SqliteConnection::connect_with(&options)
                .await
                .expect("open catalog.db to checkpoint it");
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&mut connection)
                .await
                .expect("checkpoint catalog.db");
            connection
                .close()
                .await
                .expect("close the checkpoint connection");
        });
    }
    assert!(
        !wal.exists() && !shm.exists(),
        "catalog.db-wal and catalog.db-shm are settled"
    );
}

/// The WAL tail of life 3 is replayed past the flushed sequence, on a copy of the WAL.
fn assert_wal_tail(dir: &Path) {
    let temp = tempfile::tempdir().expect("a temp dir for the WAL copy");
    let copy = temp.path().join("spans");
    crate::test_support::copy_dir(&dir.join("wal").join("spans"), &copy);
    let catalog_copy = temp.path().join("catalog.db");
    fs::copy(dir.join("catalog.db"), &catalog_copy).expect("copy catalog.db");
    let flushed: i64 = catalog_rows(
        &catalog_copy,
        "SELECT flushed_seq FROM table_state WHERE table_name = 'spans'",
    )
    .first()
    .and_then(|row| row[0].as_i64())
    .unwrap_or(0);
    let mut records = 0_u64;
    let mut seen_after = 0_u64;
    crate::wal::replay(
        &copy,
        Table::Spans,
        u64::try_from(flushed).expect("a flushed sequence is not negative"),
        false,
        &mut |seq: u64, _batch| {
            records += 1;
            if seq > u64::try_from(flushed).unwrap_or(0) {
                seen_after += 1;
            }
            Ok(())
        },
    )
    .expect("replay the spans WAL");
    assert!(
        seen_after > 0,
        "the spans WAL holds records after flushed_seq (the WAL tail); {records} records"
    );
}

/// Step 7: the read cache entry of the golden directory, one 4 KiB object of the byte pattern
/// `i % 251`, written through the cache's own API.
fn write_cache_entry(dir: &Path) {
    let spill = tempfile::tempdir().expect("a temp dir for the spill directory");
    let disk = Arc::new(
        Disk::open(vec![dir.to_path_buf()], 0, test_spill(spill.path())).expect("open the disk"),
    );
    let cache = ReadCache::open(dir.join("cache"), 1 << 30, disk, 0).expect("open the read cache");
    cache
        .set_size(CACHE_NAME, CACHE_SIZE)
        .expect("record the cached object's size");
    let bytes: Vec<u8> = (0..CACHE_SIZE)
        .map(|index| u8::try_from(index % 251).expect("a remainder below 251 fits in a byte"))
        .collect();
    cache.write_blocks(CACHE_NAME, &[(0, Bytes::from(bytes))]);
}

/// Step 8: the lock and the temp directory are not part of the directory.
fn clean_tree(dir: &Path) {
    let _ = fs::remove_file(dir.join("LOCK"));
    let _ = fs::remove_dir_all(dir.join("tmp"));
}
