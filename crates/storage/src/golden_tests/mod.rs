//! The golden data directory (track R M8, D-R-29, spec 03 § 12): a data directory written by
//! this build's scenario, and the answers of the public reads over it, which every later build
//! must keep giving.
//!
//! `build.rs` writes the directory, `dump.rs` records the reads. Nothing here touches
//! `fixtures/storage/golden/`: `write_golden_dir` writes there only when `just golden-dir` runs it.

mod build;
mod dump;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use arrow::array::StringArray;
use cetra_core::ids::ProjectId;
use cetra_core::schema::spans;
use chrono::NaiveDate;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::Value;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use uuid::Uuid;

use crate::Storage;
use crate::catalog::FileTable;
use crate::error::{CatalogError, HeaderProblem, OpenError, WalError};
use crate::format::FORMAT_VERSION;
use crate::format::tests_support::dir_digest;
use crate::test_support::{copy_dir, span_rows};
use dump::block_on;

/// The version of the scenario and of this file's format. Frozen from the first release on: a
/// later change to what `build` writes is a new number, with its code path beside the old one.
pub(super) const GENERATOR: u64 = 1;

/// Golden projects (spec in `conventions.md`: `P1`, `P2`, `P3` is dropped).
pub(super) const P1: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000a1";
pub(super) const P2: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000a2";
pub(super) const P3: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000a3";

/// The deletions of the scenario: one applied (a trace), one pending (a user).
pub(super) const DELETION_TRACE: Uuid = Uuid::from_u128(0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00d1);
pub(super) const DELETION_USER: Uuid = Uuid::from_u128(0x018f_6b4e_7c1a_7c1a_8c1a_0000_0000_00d2);

/// The blob the scenario puts: 16 bytes.
pub(super) const MEDIA: &[u8] = b"cetra golden 16";

/// The read cache entry the scenario writes (step 7 of the scenario).
pub(super) const CACHE_NAME: &str = "golden-cache.parquet";
pub(super) const CACHE_LOCATION: &str = "tables/spans/golden-cache.parquet";
pub(super) const CACHE_SIZE: u64 = 4096;

/// Nanoseconds since the Unix epoch at `seconds` after midnight UTC of `day` (`YYYY-MM-DD`).
pub(super) fn ns_at(day: &str, seconds: i64) -> i64 {
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").expect("a golden day is YYYY-MM-DD");
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .and_then(|time| time.and_utc().timestamp_nanos_opt())
        .expect("a golden day has a midnight in nanoseconds");
    midnight + seconds * 1_000_000_000
}

/// The window of every read of the dump: `[D0 midnight, D2 midnight + 1 day)`.
pub(super) fn window() -> (i64, i64) {
    (ns_at("2026-02-01", 0), ns_at("2026-02-12", 0))
}

/// Writes the scenario into `out` (which must not exist), then its `expected.json`.
pub(super) fn generate(out: &Path) {
    build::build(out);
    write_expected(out);
}

/// Writes `dir/expected.json` (pretty JSON and a trailing newline) for the directory `dir`.
fn write_expected(dir: &Path) {
    let expected = dump::expected_of(dir);
    let mut text = serde_json::to_string_pretty(&expected).expect("expected.json serializes");
    text.push('\n');
    fs::write(dir.join("expected.json"), text).expect("write expected.json");
}

/// The fresh golden directory, built once per process on first use, with its `expected.json`
/// (R8.6: every test copies from it; only `build_is_deterministic` builds another one).
struct Fresh {
    /// Keeps the temp directory alive for the process.
    _dir: tempfile::TempDir,
    golden: PathBuf,
}

static FRESH: OnceLock<Fresh> = OnceLock::new();

/// The fresh golden directory. Never write into it: copy it first. Not inside a tokio runtime:
/// building it runs its lives on runtimes of their own.
#[cfg(test)]
pub(crate) fn fresh_golden_dir() -> &'static Path {
    &FRESH
        .get_or_init(|| {
            let dir = tempfile::tempdir().expect("a temp dir for the fresh golden directory");
            let golden = dir.path().join("golden");
            generate(&golden);
            Fresh { _dir: dir, golden }
        })
        .golden
}

/// The relative paths of the files of `dir`, sorted, for an assertion message.
fn list(dir: &Path) -> Vec<String> {
    dir_digest(dir).into_iter().map(|(path, _)| path).collect()
}

#[test]
fn build_is_deterministic() {
    let started = Instant::now();
    let fresh = fresh_golden_dir();
    let temp = tempfile::tempdir().expect("a temp dir for the second build");
    let second = temp.path().join("golden");
    build::build(&second);
    let recorded: Value = serde_json::from_str(
        &fs::read_to_string(fresh.join("expected.json")).expect("the fresh expected.json"),
    )
    .expect("the fresh expected.json is JSON");
    let again = dump::expected_of(&second);
    assert_eq!(
        again, recorded,
        "a second build of the golden scenario records different reads: the scenario is not deterministic"
    );
    eprintln!("build_is_deterministic: {:?}", started.elapsed());
}

#[test]
fn build_leaves_the_expected_shape() {
    let started = Instant::now();
    let temp = tempfile::tempdir().expect("a temp dir for the copy");
    let dir = temp.path().join("golden");
    copy_dir(fresh_golden_dir(), &dir);

    // The catalog is settled: no WAL or shared memory file, and no lock.
    assert!(
        dir.join("catalog.db").is_file(),
        "catalog.db: {:?}",
        list(&dir)
    );
    assert!(
        !dir.join("catalog.db-wal").exists(),
        "catalog.db-wal is settled"
    );
    assert!(
        !dir.join("catalog.db-shm").exists(),
        "catalog.db-shm is settled"
    );
    assert!(!dir.join("LOCK").exists(), "the lock is removed");

    // The WAL: one or more segments per table, each with the format header.
    for table in ["spans", "scores"] {
        let segments = wal_segments(&dir.join("wal").join(table));
        assert!(!segments.is_empty(), "wal/{table} holds a segment");
        for segment in segments {
            let bytes = fs::read(&segment).expect("read a WAL segment");
            assert_eq!(&bytes[..4], b"LWAL", "wal/{table} segment header");
        }
    }

    // The catalog: an L0 and an L1 spans file of a live project, an obsolete file, one
    // project row, one media row, one pending and one applied deletion.
    let catalog = dir.join("catalog.db");
    let summary = dump::catalog_summary(&dir);
    let files = summary["data_files"].as_array().expect("data_files rows");
    let live = |level: u64, state: &str| {
        files.iter().any(|row| {
            row[0] == "spans" && row[1] == level && row[2] == state && row[3].as_u64() > Some(0)
        })
    };
    assert!(live(1, "active"), "an L1 spans file after compaction");
    assert!(live(0, "active"), "an L0 spans file (the WAL tail's day)");
    assert!(
        files
            .iter()
            .any(|row| row[2] == "obsolete" && row[3].as_u64() > Some(0)),
        "an obsolete file after retention and the deletion rewrite"
    );
    assert_eq!(
        summary["projects"].as_array().map(Vec::len),
        Some(1),
        "one projects row"
    );
    assert_eq!(
        summary["media"].as_array().map(Vec::len),
        Some(1),
        "one media row"
    );
    assert_eq!(
        dump::catalog_count(
            &catalog,
            "SELECT COUNT(*) FROM deletions WHERE status = 'pending'"
        ),
        1,
        "one pending deletion"
    );
    assert_eq!(
        dump::catalog_count(
            &catalog,
            "SELECT COUNT(*) FROM deletions WHERE status = 'applied'"
        ),
        1,
        "one applied deletion"
    );

    // The read cache entry: one object of the recorded size, with its size file.
    assert!(
        dir.join("cache").join(CACHE_NAME).join("size").is_file(),
        "cache/{CACHE_NAME}/size"
    );

    // Every Parquet file of the tables carries the format version (R8.4).
    let mut parquet = 0;
    for file in parquet_files(&dir.join("objects").join("tables")) {
        let bytes = fs::read(&file).expect("read a Parquet file");
        let metadata = parquet::file::metadata::ParquetMetaDataReader::new()
            .parse_and_finish(&bytes::Bytes::from(bytes))
            .expect("read a Parquet footer");
        assert_eq!(
            crate::parquet::file_format_version(&metadata),
            Some(FORMAT_VERSION),
            "the format version of {}",
            file.display()
        );
        parquet += 1;
    }
    assert!(parquet > 0, "the golden directory has Parquet files");

    // The error path reaches the golden data (R8.6): P1 has a non-zero ErrorRate, one of its traces
    // has error_count above zero, and a trace detail has a span with status `error`.
    let expected = read_expected(&dir);
    let p1 = &expected["projects"][P1];
    let error_rate = p1["kpis"]
        .as_array()
        .and_then(|kpis| kpis.iter().find(|kpi| kpi["name"] == "ErrorRate"))
        .and_then(|kpi| kpi["value"].as_f64());
    assert!(
        error_rate.is_some_and(|rate| rate > 0.0),
        "the ErrorRate of P1 is {error_rate:?}: the scenario has an error trace"
    );
    assert!(
        p1["traces"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["error_count"].as_i64() > Some(0))),
        "a P1 trace has error_count above zero"
    );
    assert!(
        p1["trace_details"].as_object().is_some_and(|details| {
            details.values().any(|detail| {
                detail["spans"]
                    .as_array()
                    .is_some_and(|spans| spans.iter().any(|span| span["status"] == "error"))
            })
        }),
        "a trace detail of P1 has a span with status error"
    );
    eprintln!("build_leaves_the_expected_shape: {:?}", started.elapsed());
}

/// The segment files of a WAL table directory, sorted.
fn wal_segments(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .expect("list a WAL directory")
        .map(|entry| entry.expect("read a WAL directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
        .collect();
    found.sort();
    found
}

/// Every `.parquet` file under `dir`, sorted.
fn parquet_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("list an objects directory") {
            let path = entry.expect("read an objects directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Writes the golden directory `CETRA_GOLDEN_OUT` names (an absolute path that must not exist),
/// with its `expected.json`. Run by `just golden-dir <name>`, which passes the name's path.
#[test]
#[ignore = "writes a fixtures directory; run by just golden-dir <version>"]
fn write_golden_dir() {
    let out = PathBuf::from(
        std::env::var_os("CETRA_GOLDEN_OUT")
            .expect("CETRA_GOLDEN_OUT names the directory to write"),
    );
    assert!(out.is_absolute(), "CETRA_GOLDEN_OUT is an absolute path");
    assert!(!out.exists(), "CETRA_GOLDEN_OUT must not exist yet");
    // A golden directory is written by the build it is named after (`release-prep` has set the
    // version already); `scratch*` is for a look and is never released (R8.8).
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .expect("CETRA_GOLDEN_OUT ends in a UTF-8 directory name");
    assert!(
        name.starts_with("scratch") || name == env!("CARGO_PKG_VERSION"),
        "{name}: the workspace is {}; a golden directory is named after the version it is written by (or scratch*)",
        env!("CARGO_PKG_VERSION")
    );
    fs::create_dir_all(out.parent().expect("CETRA_GOLDEN_OUT has a parent"))
        .expect("create the parent of CETRA_GOLDEN_OUT");
    generate(&out);
    let (files, bytes) = dir_size(&out);
    println!(
        "golden directory {}: {files} files, {bytes} bytes",
        out.display()
    );
}

/// The number of regular files under `dir` and their total size in bytes.
fn dir_size(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("list a golden directory") {
            let entry = entry.expect("read a directory entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files += 1;
                total += entry.metadata().expect("the size of a file").len();
            }
        }
    }
    (files, total)
}

/// The bound on the total size of a golden directory (R8.7): it grows the repository by its size
/// for every release.
const GOLDEN_MAX_BYTES: u64 = 1 << 20;

/// The golden directories committed under `fixtures/storage/golden/`, sorted. Plain files such as
/// `RELEASES` are skipped.
pub(crate) fn committed_dirs() -> Vec<PathBuf> {
    let root = golden_root();
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("list {}: {error}", root.display()))
        .map(|entry| entry.expect("read a golden directory entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `fixtures/storage/golden/`, the committed golden directories and `RELEASES`.
fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/storage/golden")
}

/// The versions listed in `RELEASES`, oldest first. Comment (`#`) and blank lines are skipped.
fn released_versions() -> Vec<String> {
    let path = golden_root().join("RELEASES");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_released_version_has_a_golden_directory() {
    for version in released_versions() {
        let dir = golden_root().join(&version);
        assert!(
            dir.is_dir(),
            "RELEASES lists {version}, which has no golden directory {}",
            dir.display()
        );
        let expected = read_expected(&dir);
        assert_eq!(
            expected["workspace_version"].as_str(),
            Some(version.as_str()),
            "{version}: expected.json names a different workspace_version"
        );
    }
}

#[test]
fn every_golden_directory_is_released() {
    let released = released_versions();
    for dir in committed_dirs() {
        let name = name_of(&dir);
        assert!(
            released.contains(&name),
            "{name}: a golden directory that RELEASES does not list (a scratch* directory: delete it)"
        );
    }
}

/// The basename of a golden directory, for assertion messages.
fn name_of(dir: &Path) -> String {
    dir.file_name().map_or_else(
        || dir.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `expected.json` of `dir`, which must name a generator this build knows (step 1).
fn read_expected(dir: &Path) -> Value {
    let name = name_of(dir);
    let text = fs::read_to_string(dir.join("expected.json"))
        .unwrap_or_else(|error| panic!("{name}: step 1: read expected.json: {error}"));
    let expected: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{name}: step 1: expected.json is not JSON: {error}"));
    let generator = expected["generator"].as_u64();
    assert_eq!(
        generator,
        Some(GENERATOR),
        "{name}: step 1: expected.json names generator {generator:?}, which this build does not know"
    );
    expected
}

/// `PRAGMA user_version` of the catalog file `file`.
async fn user_version(file: &Path) -> i64 {
    let mut connection = SqliteConnectOptions::new()
        .filename(file)
        .connect()
        .await
        .unwrap_or_else(|error| panic!("open a copy of catalog.db: {error}"));
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("read PRAGMA user_version: {error}"));
    connection
        .close()
        .await
        .unwrap_or_else(|error| panic!("close a copy of catalog.db: {error}"));
    version
}

/// Sets `PRAGMA user_version` of the catalog file `file` (of a copy only).
async fn set_user_version(file: &Path, version: i64) {
    let mut connection = SqliteConnectOptions::new()
        .filename(file)
        .connect()
        .await
        .unwrap_or_else(|error| panic!("open a copy of catalog.db: {error}"));
    // The version is a number of this test, not data: a PRAGMA takes no bind parameter.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {version}"
    )))
    .execute(&mut connection)
    .await
    .unwrap_or_else(|error| panic!("set PRAGMA user_version: {error}"));
    connection
        .close()
        .await
        .unwrap_or_else(|error| panic!("close a copy of catalog.db: {error}"));
}

/// Every WAL segment under `copy/wal/<table>/`, for each table directory, sorted.
fn all_wal_segments(copy: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for table in fs::read_dir(copy.join("wal")).expect("list the wal directory") {
        let table = table.expect("read the wal directory").path();
        found.extend(wal_segments(&table));
    }
    found.sort();
    found
}

/// The first 8 bytes of the segment `path` (fewer if the file is shorter).
fn segment_header(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let file = fs::File::open(path).expect("open a WAL segment");
    let mut header = Vec::new();
    file.take(8)
        .read_to_end(&mut header)
        .expect("read a WAL segment header");
    header
}

/// Rows of `sql` on the catalog of `file`, read on a blocking thread (the reader of `catalog_rows`
/// starts a runtime, which an async test may not).
async fn catalog_rows_of(file: PathBuf, sql: &'static str) -> Vec<Value> {
    tokio::task::spawn_blocking(move || dump::catalog_rows(&file, sql))
        .await
        .expect("the catalog query task")
}

/// Checks the golden directory `dir` against the current build (R8.7, plan § check_dir). `dir` is
/// never written: everything runs on a copy, and the open (step 6) runs after the catalog and the
/// file checks (steps 3 to 5), which see the directory as it was written: the open collects the
/// obsolete files older than `gc.grace`, and an aged directory's obsolete files all are.
async fn check_dir(dir: &Path) {
    let started = Instant::now();
    let name = name_of(dir);

    // 1. expected.json.
    let expected = read_expected(dir);
    let format = expected["format_version"]
        .as_u64()
        .and_then(|version| u16::try_from(version).ok())
        .unwrap_or_else(|| panic!("{name}: step 1: expected.json has no format_version"));

    // 2. A copy, in a temp directory.
    let temp = tempfile::tempdir().expect("a temp dir for the copy");
    let copy = temp.path().join("golden");
    copy_dir(dir, &copy);

    // 3. The marker, and every WAL segment's header.
    let marker = user_version(&copy.join("catalog.db")).await;
    assert_eq!(
        marker,
        i64::from(format),
        "{name}: step 3: PRAGMA user_version of catalog.db"
    );
    for segment in all_wal_segments(&copy) {
        let header = segment_header(&segment);
        assert!(
            header.len() == 8 && header[..4] == *b"LWAL",
            "{name}: step 3: a WAL segment does not start with the LWAL header"
        );
        assert_eq!(
            u16::from_le_bytes([header[4], header[5]]),
            format,
            "{name}: step 3: the format version of a WAL segment"
        );
    }

    // 4. The catalog summary.
    let summary = tokio::task::spawn_blocking({
        let copy = copy.clone();
        move || dump::catalog_summary(&copy)
    })
    .await
    .expect("the catalog summary task");
    assert_eq!(
        summary, expected["catalog"],
        "{name}: step 4: the catalog summary differs from expected.json"
    );

    // 5. The data files: every active or obsolete object is present, and every active Parquet
    // file's footer carries the format version.
    let files = catalog_rows_of(
        copy.join("catalog.db"),
        "SELECT path, state FROM data_files WHERE state IN ('active', 'obsolete')",
    )
    .await;
    let mut missing = 0;
    let mut footers = 0;
    for row in &files {
        let path = copy
            .join("objects")
            .join(row[0].as_str().expect("a data file path"));
        if !path.is_file() {
            missing += 1;
            continue;
        }
        if row[1] == "active" {
            use parquet::file::reader::FileReader as _;
            let reader = parquet::file::reader::SerializedFileReader::new(
                fs::File::open(&path).expect("open an active data file"),
            )
            .unwrap_or_else(|error| {
                panic!("{name}: step 5: read an active Parquet footer: {error}")
            });
            assert_eq!(
                crate::parquet::file_format_version(reader.metadata()),
                Some(format),
                "{name}: step 5: the format version of an active Parquet file"
            );
            footers += 1;
        }
    }
    assert_eq!(
        missing, 0,
        "{name}: step 5: data_files rows whose object is missing under objects/"
    );
    assert!(
        footers > 0,
        "{name}: step 5: the directory has active Parquet files"
    );

    // 6. The open, with the config of `build`.
    let storage = Storage::open(build::golden_config(&copy))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 6: Storage::open: {error}"));

    // 7. The reads answer what expected.json records (the catalog, and the format and workspace
    // versions, are not reads).
    let first = dump::dump(&storage).await;
    for (key, value) in expected.as_object().expect("expected.json is an object") {
        if matches!(
            key.as_str(),
            "format_version" | "workspace_version" | "catalog"
        ) {
            continue;
        }
        assert_eq!(
            first.get(key),
            Some(value),
            "{name}: step 7: the read {key} differs from expected.json"
        );
    }

    // 8. New writes on the old directory: one probe trace, flushed, compacted, deletions applied.
    let p1: ProjectId = P1.parse().expect("a golden project id");
    let probe_start = expected["write_probe"]["start_time"]
        .as_i64()
        .unwrap_or_else(|| panic!("{name}: step 8: expected.json has no write_probe.start_time"));
    let probe = build::probe(probe_start);
    let probe_trace = probe[0].trace_id.to_string();
    storage
        .append_spans(p1, span_rows(&probe))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 8: append the probe trace: {error}"));
    storage
        .flush_now(std::time::Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 8: flush: {error}"));
    storage
        .compact(ns_at("2026-02-12", 0))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 8: compact: {error}"));
    // The rows of u-2 are in the active files before the deletion is applied, and not after it.
    // Only trace 2 (day D1) is counted before: the compaction above rewrote day D2 and dropped
    // trace 4, which the pending deletion's tombstones cover (spec 03 § 7: flush and compaction
    // apply the tombstone set).
    // The physical check reads the files: a read alone proves nothing, because a read hides the
    // rows of a pending deletion whether or not the deletion was applied.
    let before = active_user_rows(&storage, p1, &copy, "u-2");
    assert!(
        before > 0,
        "{name}: step 8: no active file holds a row of u-2 before the deletion"
    );
    storage
        .apply_deletions(ns_at("2026-02-12", 0))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 8: apply deletions: {error}"));
    let after = active_user_rows(&storage, p1, &copy, "u-2");
    assert_eq!(
        after, 0,
        "{name}: step 8: the applied user deletion of u-2 left {after} rows in the active files of P1"
    );
    let second = dump::dump(&storage).await;

    let second_p1 = &second["projects"][P1];
    let probe_detail = &second_p1["trace_details"][&probe_trace];
    assert_eq!(
        probe_detail["spans"].as_array().map(Vec::len),
        Some(2),
        "{name}: step 8: the probe trace is readable with its two spans"
    );
    for project in [P1, P2] {
        let old = expected["projects"][project]["trace_details"]
            .as_object()
            .expect("expected trace_details");
        for (id, detail) in old {
            assert_eq!(
                second["projects"][project]["trace_details"].get(id),
                Some(detail),
                "{name}: step 8: the trace_detail of an old trace differs from expected.json"
            );
        }
    }
    let mut listed = second_p1["traces"].as_array().expect("trace rows").clone();
    let probes = listed
        .iter()
        .filter(|row| row["trace_id"] == probe_trace.as_str())
        .count();
    assert_eq!(probes, 1, "{name}: step 8: the probe trace is listed once");
    listed.retain(|row| row["trace_id"] != probe_trace.as_str());
    assert_eq!(
        Value::Array(listed),
        expected["projects"][P1]["traces"],
        "{name}: step 8: trace_list is not expected.json's plus exactly the probe trace"
    );
    // The second dump is the first plus exactly the probe, with two exceptions the probe and the
    // applied deletion explain: the user deletion of u-2 is now applied (only its status changes),
    // and the KPIs are aggregates over the traces, so they change by the probe's contribution.
    let probe_row = second_p1["traces"]
        .as_array()
        .expect("trace rows")
        .iter()
        .find(|row| row["trace_id"] == probe_trace.as_str())
        .expect("the probe trace row")
        .clone();
    let mut want = expected.clone();
    let want_map = want.as_object_mut().expect("expected.json is an object");
    for key in ["format_version", "workspace_version", "catalog"] {
        want_map.remove(key);
    }
    want["deletions"][DELETION_USER.to_string()] = Value::from("applied");
    want["projects"][P1]["kpis"] = dump::kpis_with_probe(
        &expected["projects"][P1]["kpis"],
        expected["projects"][P1]["traces"]
            .as_array()
            .expect("the expected trace rows"),
        &probe_row,
    );
    let mut got = without_probe(second.clone(), &probe_trace);
    let got_map = got.as_object_mut().expect("the dump is an object");
    for key in ["format_version", "workspace_version"] {
        got_map.remove(key);
    }
    assert_eq!(
        got, want,
        "{name}: step 8: the second dump is not expected.json plus exactly the probe trace, with the u-2 deletion applied"
    );

    // 9. Shutdown.
    storage
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{name}: step 9: shutdown: {error}"));
    eprintln!("check_dir {name}: {:?}", started.elapsed());
}

/// The rows of `user` in the Parquet files the catalog lists as active for `project`, counted in
/// the files of the directory `dir` (its `objects` directory). Read from the files, not through
/// a read API: a read hides the rows of a pending deletion.
fn active_user_rows(storage: &Storage, project: ProjectId, dir: &Path, user: &str) -> usize {
    let mut rows = 0;
    for file in storage.catalog_files(FileTable::Spans) {
        if file.project != project {
            continue;
        }
        let path = dir.join("objects").join(&file.path);
        let handle = fs::File::open(&path)
            .unwrap_or_else(|error| panic!("open an active spans file: {error}"));
        let reader = ParquetRecordBatchReaderBuilder::try_new(handle)
            .unwrap_or_else(|error| panic!("read an active spans file: {error}"))
            .build()
            .unwrap_or_else(|error| panic!("read an active spans file: {error}"));
        for batch in reader {
            let batch = batch.unwrap_or_else(|error| panic!("read a spans batch: {error}"));
            let users = batch
                .column_by_name(spans::USER_ID)
                .and_then(|column| column.as_any().downcast_ref::<StringArray>())
                .unwrap_or_else(|| panic!("a spans file has a Utf8 {} column", spans::USER_ID));
            rows += users.iter().filter(|value| *value == Some(user)).count();
        }
    }
    rows
}

/// A dump without the probe trace: its row, its detail, its spans, and its user and session.
fn without_probe(mut dump: Value, probe_trace: &str) -> Value {
    let project = &mut dump["projects"][P1];
    if let Some(rows) = project["traces"].as_array_mut() {
        rows.retain(|row| row["trace_id"] != probe_trace);
    }
    if let Some(details) = project["trace_details"].as_object_mut() {
        details.remove(probe_trace);
    }
    if let Some(rows) = project["spans"].as_array_mut() {
        rows.retain(|row| row["trace_id"] != probe_trace);
    }
    if let Some(rows) = project["sessions"].as_array_mut() {
        rows.retain(|row| row["session_id"] != "s-probe");
    }
    if let Some(rows) = project["users"].as_array_mut() {
        rows.retain(|row| row["user_id"] != "u-probe");
    }
    dump
}

#[test]
fn a_fresh_golden_directory_opens() {
    block_on(check_dir(fresh_golden_dir()));
}

#[test]
fn the_committed_golden_directories_open() {
    let dirs = committed_dirs();
    if dirs.is_empty() {
        eprintln!("the_committed_golden_directories_open: no golden directory is committed yet");
        return;
    }
    for dir in dirs {
        block_on(check_dir(&dir));
    }
}

#[test]
fn golden_directories_stay_small() {
    let started = Instant::now();
    let mut dirs = committed_dirs();
    dirs.push(fresh_golden_dir().to_path_buf());
    for dir in dirs {
        let name = name_of(&dir);
        let (files, bytes) = dir_size(&dir);
        eprintln!("golden_directories_stay_small: {name}: {files} files, {bytes} bytes");
        assert!(
            bytes <= GOLDEN_MAX_BYTES,
            "{name}: {bytes} bytes, over {GOLDEN_MAX_BYTES}: a golden directory grows the repository by its size for every release: shrink the scenario in golden_tests/build.rs; do not raise the limit without the coordinator"
        );
    }
    eprintln!("golden_directories_stay_small: {:?}", started.elapsed());
}

#[test]
fn a_newer_golden_directory_is_refused() {
    let started = Instant::now();
    let mut dirs = committed_dirs();
    if dirs.is_empty() {
        dirs.push(fresh_golden_dir().to_path_buf());
    }
    for dir in dirs {
        let name = name_of(&dir);
        let temp = tempfile::tempdir().expect("a temp dir for the copy");
        let copy = temp.path().join("golden");
        copy_dir(&dir, &copy);
        let catalog = copy.join("catalog.db");

        // A newer marker: refused by the catalog check, before anything is written.
        block_on(set_user_version(&catalog, i64::from(FORMAT_VERSION) + 1));
        let before = dir_digest(&copy);
        let refused = block_on(Storage::open(build::golden_config(&copy)));
        let Err(error) = refused else {
            panic!("{name}: a directory with a newer marker opened");
        };
        assert!(
            matches!(
                error,
                OpenError::Catalog(CatalogError::FormatTooNew { found, .. })
                    if found == i64::from(FORMAT_VERSION) + 1
            ),
            "{name}: a newer marker is refused with FormatTooNew, got {error}"
        );
        assert_eq!(
            dir_digest(&copy),
            before,
            "{name}: a refused open changed the directory (marker)"
        );

        // A newer first WAL segment, with the marker restored: refused by the WAL check.
        block_on(set_user_version(&catalog, i64::from(FORMAT_VERSION)));
        let segment = wal_segments(&copy.join("wal").join("spans"))
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("{name}: the spans WAL has a segment"));
        let mut bytes = fs::read(&segment).expect("read the first WAL segment");
        bytes[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        fs::write(&segment, bytes).expect("write the first WAL segment");
        let before = dir_digest(&copy);
        let refused = block_on(Storage::open(build::golden_config(&copy)));
        let Err(error) = refused else {
            panic!("{name}: a directory with a newer WAL segment opened");
        };
        assert!(
            matches!(
                error,
                OpenError::WalReplay(WalError::Header {
                    problem: HeaderProblem::NewerFormat { found, .. },
                    ..
                }) if found == FORMAT_VERSION + 1
            ),
            "{name}: a newer WAL segment is refused with NewerFormat, got {error}"
        );
        assert_eq!(
            dir_digest(&copy),
            before,
            "{name}: a refused open changed the directory (WAL header)"
        );
    }
    eprintln!(
        "a_newer_golden_directory_is_refused: {:?}",
        started.elapsed()
    );
}
