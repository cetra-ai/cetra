//! In-memory tables built from `fixtures/api/dataset` for query tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use cetra_column::{Batch, FlatData};
use cetra_core::batch::{scores_to_batch, spans_to_batch};
use cetra_core::enums::{SpanKind, SpanStatus};
use cetra_core::ids::TraceId;
use cetra_core::schema::{scores_schema, spans_schema, traces_schema};
use cetra_core::score::Score;
use cetra_core::span::Span;
use serde_json::{Map, Value};

use crate::catalog::Catalog;
use crate::config::StorageConfig;
use crate::engine::DAY_NS;
use crate::memtable::Memtable;
use crate::read_state::ReadState;
use crate::reads::columns::active_rows;
use crate::reads::{
    Agg, Bucket, BucketWidth, Measure, Shape, WidgetData, WidgetQuery, WidgetRequest, WidgetSource,
    widget_query,
};
use crate::sql::{ExportFormat, SQL_MAX_ROWS, SqlRequest};
use crate::verify::VerifyQueue;
use crate::views::{ProjectQuery, QueryContext, TimeWindow};
use crate::wal::Table;
use crate::{EngineContext, QeRuntime, QueryOptions, Storage};

/// Turns core dumps off for this process. A crash child calls it before it can abort, so an
/// abort writes no `core.*` file to the working directory of the run.
pub(crate) fn no_core_dumps() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut limit = getrlimit(Resource::Core);
    limit.current = Some(0);
    // Best effort: a failure leaves the default, and the child still aborts as it would.
    let _ = setrlimit(Resource::Core, limit);
}

fn dataset_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/api/dataset")
}

/// The test trace id of [`span_row`] and [`score_row`]: the 2 big-endian bytes
/// of `trace + 1`, repeated 8 times. The `+ 1` keeps the id from being all
/// zero, which the id type rejects.
fn trace_id_of(trace: u16) -> TraceId {
    assert!(trace < u16::MAX, "trace must be below u16::MAX");
    let mut trace_bytes = [0_u8; 16];
    let pair = (trace + 1).to_be_bytes();
    for chunk in trace_bytes.as_chunks_mut::<2>().0 {
        chunk.copy_from_slice(&pair);
    }
    TraceId::from_bytes(trace_bytes).unwrap()
}

fn read_text(name: &str) -> String {
    let path = dataset_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The rows stored in one catalogued file's Statam object (and optional sibling `.stpk` pack),
/// under `objects/`.
pub(crate) fn object_rows(dir: &Path, file: &crate::catalog::FileMeta) -> Batch {
    let bytes = fs::read(dir.join("objects").join(&file.path)).unwrap();
    let pack = file.pack.as_ref().map(|p| {
        let pack_bytes = fs::read(dir.join("objects").join(&p.path)).unwrap();
        (p.id, Bytes::from(pack_bytes))
    });
    statam_rows(Bytes::from(bytes), pack)
}

/// The rows of one Statam object (and optional sibling `.stpk` pack), given its bytes.
pub(crate) fn statam_rows(bytes: Bytes, pack: Option<([u8; 16], Bytes)>) -> Batch {
    let meta = cetra_statam::StatamFileMeta::read_from(
        &cetra_statam::SliceReader::new(bytes.clone()),
        None,
    )
    .unwrap();
    let schema = match meta.schema_meta.table_kind {
        cetra_statam::TableKind::Traces => traces_schema(),
        cetra_statam::TableKind::Scores => scores_schema(),
        cetra_statam::TableKind::Spans | cetra_statam::TableKind::Generic => spans_schema(),
    };
    crate::engine::statam_source::tests::read_stored_statam(bytes, pack, schema)
}

/// The `trace_id` bytes of every row of a stored batch.
pub(crate) fn trace_ids_of(rows: &Batch) -> Vec<[u8; 16]> {
    let column = rows
        .column_by_name(cetra_core::schema::spans::TRACE_ID)
        .unwrap();
    let flat = column.materialize(None).unwrap();
    match flat.data() {
        FlatData::FixedBytes16(arr) => active_rows(rows).map(|row| arr[row]).collect(),
        other => panic!("unexpected trace_id flat data: {other:?}"),
    }
}

pub(crate) fn dataset_spans() -> Vec<Span> {
    serde_json::from_str(&read_text("spans.json"))
        .unwrap_or_else(|error| panic!("spans.json: {error}"))
}

pub(crate) fn dataset_scores() -> Vec<Score> {
    serde_json::from_str(&read_text("scores.json"))
        .unwrap_or_else(|error| panic!("scores.json: {error}"))
}

/// Last writer wins per `score_id` by `(updated_at, ingest_time)`. Deleted winners are dropped.
pub(crate) fn live_scores(scores: &[Score]) -> Vec<Score> {
    let mut latest = BTreeMap::new();
    for score in scores {
        let replace = latest.get(&score.score_id).is_none_or(|current: &Score| {
            (score.updated_at, score.ingest_time) > (current.updated_at, current.ingest_time)
        });
        if replace {
            latest.insert(score.score_id, score.clone());
        }
    }
    latest
        .into_values()
        .filter(|score| !score.deleted)
        .collect()
}

/// Copies the directory `from` to `to` (which may not exist), every file and subdirectory of it.
pub(crate) fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the copy of a directory");
    for entry in fs::read_dir(from).expect("list a directory to copy") {
        let entry = entry.expect("read a directory entry");
        let target = to.join(entry.file_name());
        if entry
            .file_type()
            .expect("the type of a directory entry")
            .is_dir()
        {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a file of a directory");
        }
    }
}

/// One span of the fixed test project, with the other fields as `wal::spans_keyed`
/// sets them.
///
/// `trace_id` is the 2 big-endian bytes of `trace + 1`, repeated 8 times;
/// `span_id` is 8 bytes of `span + 1`. The `+ 1` keeps the ids from being all
/// zero, which the id types reject. `trace` must be below `u16::MAX` and `span`
/// below `u8::MAX`; the builder panics otherwise. A test that needs another
/// field (`parent_span_id`, `input`) sets it on the returned `Span`.
pub(crate) fn span_row(trace: u16, span: u8, start: i64, ingest: i64) -> Span {
    assert!(span < u8::MAX, "span must be below u8::MAX");
    let project_id: cetra_core::ids::ProjectId =
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
    let span_bytes = [span + 1; 8];
    Span {
        project_id,
        trace_id: trace_id_of(trace),
        span_id: cetra_core::ids::SpanId::from_bytes(span_bytes).unwrap(),
        parent_span_id: None,
        name: "root".to_owned(),
        span_kind: SpanKind::Span,
        start_time: start,
        end_time: start.saturating_add(100),
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
        prompt_name: None,
        prompt_version: None,
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
        ingest_time: ingest,
        schema_version: 1,
    }
}

/// `spans_to_batch(spans)`, unwrapped.
pub(crate) fn span_rows(spans: &[Span]) -> Batch {
    spans_to_batch(spans).unwrap_or_else(|error| panic!("spans batch: {error}"))
}

/// One span of `trace` whose `input` is `len` bytes of incompressible printable
/// ASCII: SplitMix64 output, each byte mapped to `b'!' + b % 94`. Its WAL record
/// is about `len` bytes, whatever the compression.
pub(crate) fn big_span_batch(trace: u16, len: usize) -> Batch {
    let mut state = 0x5eed_u64;
    let input: String = (0..len)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let offset = u8::try_from(z % 94).expect("a remainder of 94 fits in a byte");
            char::from(b'!' + offset)
        })
        .collect();
    let mut span = span_row(trace, 0, 1_000, 1_000);
    span.input = Some(input);
    span_rows(&[span])
}

/// One score of the fixed test project on the given trace, the score twin of
/// [`span_row`]: `trace_id` is the 2 big-endian bytes of `trace + 1`, repeated.
/// A test that needs `session_id`, `user_id` or another target sets it on the
/// returned `Score`; a session score has `target: ScoreTarget::Session { .. }`
/// and no trace id.
pub(crate) fn score_row(
    id: u128,
    trace: u16,
    created_at: i64,
    updated_at: i64,
    ingest: i64,
    deleted: bool,
    name: &str,
) -> Score {
    Score {
        project_id: "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap(),
        score_id: uuid::Uuid::from_u128(id),
        target: cetra_core::score::ScoreTarget::Trace {
            trace_id: trace_id_of(trace),
        },
        session_id: None,
        user_id: None,
        name: name.to_owned(),
        value: cetra_core::score::ScoreValue::Numeric(0.5),
        reason: None,
        source: cetra_core::enums::ScoreSource::Api,
        evaluator_id: None,
        rule_id: None,
        experiment_id: None,
        dataset_item_id: None,
        queue_id: None,
        author_user_id: None,
        metadata: BTreeMap::new(),
        created_at,
        updated_at,
        deleted,
        ingest_time: ingest,
    }
}

/// `scores_to_batch(scores)`, unwrapped.
pub(crate) fn score_rows(scores: &[Score]) -> Batch {
    scores_to_batch(scores).unwrap_or_else(|error| panic!("scores batch: {error}"))
}

fn fallback_runtime_handle() -> tokio::runtime::Handle {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test fallback runtime")
    })
    .handle()
    .clone()
}

pub(crate) fn example_query() -> ProjectQuery {
    let spans = dataset_spans();
    let scores = dataset_scores();
    let project = spans[0].project_id;
    let mut mt_spans = Memtable::new(Table::Spans);
    mt_spans.insert(project, span_rows(&spans), 1).unwrap();
    let mut mt_scores = Memtable::new(Table::Scores);
    mt_scores.insert(project, score_rows(&scores), 1).unwrap();
    let state = ReadState {
        spans: mt_spans.snapshot(),
        scores: mt_scores.snapshot(),
        catalog: Arc::new(Catalog::empty()),
        ..ReadState::empty()
    };
    let handle =
        tokio::runtime::Handle::try_current().unwrap_or_else(|_| fallback_runtime_handle());
    let engine = EngineContext::from_state(
        &state,
        Arc::new(object_store::memory::InMemory::new()),
        project,
        &QueryOptions::new(None, None),
        QeRuntime::for_test(),
        DAY_NS,
        &Arc::new(VerifyQueue::default()),
        handle,
    )
    .expect("example_query");
    ProjectQuery::from_engine(engine)
}

pub(crate) async fn rows(ctx: &QueryContext, sql: &str) -> Vec<Map<String, Value>> {
    use futures::StreamExt;

    let mut stream = ctx
        .sql_export(
            SqlRequest {
                query: sql,
                params: &Map::new(),
                limit: SQL_MAX_ROWS,
                timeout: Duration::from_secs(30),
            },
            ExportFormat::Ndjson,
        )
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.unwrap_or_else(|error| panic!("{sql}: {error}"));
        buf.extend_from_slice(&bytes);
    }
    let text = String::from_utf8(buf).expect("valid utf-8 ndjson");
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Map<String, Value>>(line).expect("valid ndjson row"))
        .collect()
}

pub(crate) fn trace_batch(spans: &[Span]) -> Batch {
    crate::partials::partials_from_spans_batch(&span_rows(spans))
        .unwrap_or_else(|error| panic!("trace batch: {error}"))
}

/// One storage life on a runtime of its own, ended like a crash (D-R-4): the
/// storage is dropped without `shutdown`, then the runtime is shut down, so no
/// task of this life can touch the directory afterwards.
pub(crate) fn crashed_life<T>(config: &StorageConfig, body: impl AsyncFnOnce(&Storage) -> T) -> T {
    let runtime = life_runtime();
    let storage = runtime
        .block_on(Storage::open(config.clone()))
        .expect("open storage for a life");
    let output = runtime.block_on(body(&storage));
    drop(storage);
    runtime.shutdown_timeout(Duration::from_secs(10));
    output
}

/// The same life, ended with `shutdown`.
pub(crate) fn clean_life<T>(config: &StorageConfig, body: impl AsyncFnOnce(&Storage) -> T) -> T {
    let runtime = life_runtime();
    let storage = runtime
        .block_on(Storage::open(config.clone()))
        .expect("open storage for a life");
    let output = runtime.block_on(body(&storage));
    runtime
        .block_on(storage.shutdown())
        .expect("clean shutdown");
    drop(storage);
    runtime.shutdown_timeout(Duration::from_secs(10));
    output
}

/// The runtime `Objects` and `Storage` run on in a life (as M1's `open_life`).
fn life_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for a life")
}

/// Object keys under `objects/tables` and `objects/media` (temp names
/// excluded) that no `data_files`, `staged_objects` or `media` row names.
pub(crate) async fn orphan_objects(data_dir: &Path) -> Vec<String> {
    let known = known_names(data_dir).await;
    let objects = data_dir.join("objects");
    let mut keys = Vec::new();
    for table_dir in ["tables", "media"] {
        collect_object_keys(&objects.join(table_dir), &objects, &mut keys);
    }
    keys.sort();
    keys.retain(|key| !known.contains(key));
    keys
}

/// Opens `catalog.db` read-only, on a connection of its own.
async fn open_catalog(data_dir: &Path) -> sqlx::SqliteConnection {
    use sqlx::ConnectOptions;
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(data_dir.join("catalog.db"))
        .read_only(true)
        .connect()
        .await
        .expect("open catalog.db read-only")
}

/// Every object name the catalog knows, in any state: `data_files` paths, `staged_objects` paths
/// and every media key (N's B5 "known").
async fn known_names(data_dir: &Path) -> BTreeSet<String> {
    let mut connection = open_catalog(data_dir).await;
    sqlx::query_scalar::<_, String>(
        "SELECT path FROM data_files
         UNION SELECT path FROM staged_objects
         UNION SELECT 'media/' || project_id || '/' || sha256 FROM media",
    )
    .fetch_all(&mut connection)
    .await
    .expect("read the object names of the catalog")
    .into_iter()
    .collect()
}

/// The names the catalog needs in the store: active data files, and the media of projects that
/// are not dropping or dropped (N's B5 "needed").
async fn needed_names(data_dir: &Path) -> BTreeSet<String> {
    let mut connection = open_catalog(data_dir).await;
    sqlx::query_scalar::<_, String>(
        "SELECT path FROM data_files WHERE state = 'active'
         UNION SELECT 'media/' || m.project_id || '/' || m.sha256 FROM media m
         WHERE NOT EXISTS (
             SELECT 1 FROM projects p
             WHERE p.project_id = m.project_id AND p.state IN ('dropping', 'dropped'))",
    )
    .fetch_all(&mut connection)
    .await
    .expect("read the needed object names of the catalog")
    .into_iter()
    .collect()
}

/// What [`collect_orphans_dry_run`] counts: the fields of N's B5 `OrphanReport` with the same
/// names.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OrphanCounts {
    pub(crate) orphans: u64,
    pub(crate) missing: u64,
    pub(crate) missing_media: u64,
}

/// Stand-in for N's B5 `Storage::collect_orphans(ZERO, dry_run = true)` until B5 merges (R4.4).
/// It lists `tables/` and `media/` on `store`, and compares the keys with the catalog in
/// `data_dir`. Every unknown object is an orphan: there is no minimum age yet. B5's
/// `collect_orphans` brings the age filter, and the model test then counts the unknown objects
/// younger than it. Like B5, it reads the needed names before listing and the known names after.
pub(crate) async fn collect_orphans_dry_run(
    data_dir: &Path,
    store: &dyn object_store::ObjectStore,
) -> OrphanCounts {
    use futures::TryStreamExt;
    let mut missing = needed_names(data_dir).await;
    let mut listed = Vec::new();
    for prefix in ["tables", "media"] {
        let objects: Vec<object_store::ObjectMeta> = store
            .list(Some(&object_store::path::Path::from(prefix)))
            .try_collect()
            .await
            .expect("list the object store");
        listed.extend(objects.into_iter().map(|meta| meta.location.to_string()));
    }
    let known = known_names(data_dir).await;
    let mut counts = OrphanCounts::default();
    for key in listed {
        if is_temp_key(&key) {
            continue;
        }
        missing.remove(&key);
        if !known.contains(&key) {
            counts.orphans += 1;
        }
    }
    for key in missing {
        if key.starts_with("tables/") {
            counts.missing += 1;
        } else {
            counts.missing_media += 1;
        }
    }
    counts
}

/// Rows of `data_files` in state `obsolete`. A settled store has none: GC deletes each such file
/// and its row together.
pub(crate) async fn obsolete_rows(data_dir: &Path) -> i64 {
    let mut connection = open_catalog(data_dir).await;
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'")
        .fetch_one(&mut connection)
        .await
        .expect("count the obsolete rows of the catalog")
}

/// A temp name, as `Objects` writes them: a dot-name with `.tmp-` in it.
fn is_temp_key(key: &str) -> bool {
    let name = key.rsplit('/').next().unwrap_or(key);
    name.starts_with('.') && name.contains(".tmp-")
}

/// Adds the key of every non-temp file under `dir`, relative to `objects`.
fn collect_object_keys(dir: &Path, objects: &Path, keys: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_object_keys(&path, objects, keys);
        } else if kind.is_file() {
            let Ok(key) = path.strip_prefix(objects) else {
                continue;
            };
            let key = key.to_string_lossy().into_owned();
            if !is_temp_key(&key) {
                keys.push(key);
            }
        }
    }
}

mod chaos;
pub(crate) use chaos::{Call, ChaosStore, Fault, Rates, Status, UploadState};

/// The headline KPIs of `window`, in the dashboard's order: traces, error rate, p95 latency, cost
/// and total tokens. Each is its value and its previous-window value. One widget total of the
/// five measures, with no filter, evaluation traces excluded and experiments kept.
pub(crate) async fn headline_kpis(
    ctx: &QueryContext,
    window: TimeWindow,
) -> Result<Vec<(Option<f64>, Option<f64>)>, String> {
    let query = WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![
            Measure {
                agg: Agg::Count,
                field: None,
                condition: None,
            },
            Measure {
                agg: Agg::Rate,
                field: None,
                condition: Some("error_count > 0".to_owned()),
            },
            Measure {
                agg: Agg::P95,
                field: Some("duration_ns".to_owned()),
                condition: None,
            },
            Measure {
                agg: Agg::Sum,
                field: Some("cost_total".to_owned()),
                condition: None,
            },
            Measure {
                agg: Agg::Sum,
                field: Some("total_tokens".to_owned()),
                condition: None,
            },
        ],
        breakdown: None,
    };
    let data = widget_query(
        ctx,
        WidgetRequest {
            window,
            bucket: Bucket::Fixed(BucketWidth::D1),
            dashboard_filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: true,
            query: &query,
            shape: Shape::Total,
            compare: true,
            timeout: Duration::from_secs(30),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let WidgetData::Total { rows } = data else {
        return Err("the KPI total answered a series".to_owned());
    };
    let [row] = rows.as_slice() else {
        return Err(format!(
            "the KPI total has {} rows, expected one",
            rows.len()
        ));
    };
    let previous = row
        .previous
        .as_ref()
        .ok_or_else(|| "the KPI total has no previous values".to_owned())?;
    Ok(row
        .values
        .iter()
        .zip(previous)
        .map(|(&value, &previous)| (value, previous))
        .collect())
}
