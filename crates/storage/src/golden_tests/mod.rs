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

use chrono::NaiveDate;
use serde_json::Value;
use uuid::Uuid;

use crate::format::FORMAT_VERSION;
use crate::format::tests_support::dir_digest;
use crate::test_support::copy_dir;

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
pub(super) const MEDIA: &[u8] = b"llmobs golden 16";

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

/// The fresh golden directory. Never write into it: copy it first.
fn fresh_golden_dir() -> &'static Path {
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

/// Writes the golden directory `LLMOBS_GOLDEN_OUT` names (an absolute path that must not exist),
/// with its `expected.json`. Run by `just golden-dir <name>`, which passes the name's path.
#[test]
#[ignore = "writes a fixtures directory; run by just golden-dir <version>"]
fn write_golden_dir() {
    let out = PathBuf::from(
        std::env::var_os("LLMOBS_GOLDEN_OUT")
            .expect("LLMOBS_GOLDEN_OUT names the directory to write"),
    );
    assert!(out.is_absolute(), "LLMOBS_GOLDEN_OUT is an absolute path");
    assert!(!out.exists(), "LLMOBS_GOLDEN_OUT must not exist yet");
    // `fixtures/storage/golden/` may not exist yet (it is first created here, before R8.7's RELEASES).
    fs::create_dir_all(out.parent().expect("LLMOBS_GOLDEN_OUT has a parent"))
        .expect("create the parent of LLMOBS_GOLDEN_OUT");
    generate(&out);
    let size = dir_bytes(&out);
    println!("golden directory {}: {size} bytes", out.display());
}

/// The total size of the regular files under `dir`.
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("list a golden directory") {
            let entry = entry.expect("read a directory entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                total += entry.metadata().expect("the size of a file").len();
            }
        }
    }
    total
}
