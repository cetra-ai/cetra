//! The golden server directory (track SF, SF6, D-R-38, spec 04 § Format versions): an online
//! backup of a scenario server, the admin socket lines that made it, and `expected.json`, what a
//! restore of it with any later build must answer.
//!
//! `build.rs` writes the directory, `dump.rs` records the answers. Nothing here touches
//! `fixtures/server/golden/`: `write_server_golden_dir` writes there only when `just golden-dir`
//! runs it.

mod build;
mod dump;

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};
use uuid::Uuid;

use crate::admin::{self, AdminError};
use crate::admin_socket::decode_response;
use crate::clock::Clock;
use crate::crypto::{self, Aad, CheckOutcome, SecretBox};
use crate::meta::connections::{self, ConnectionFields};
use crate::meta::test_digest::dir_digest;
use crate::meta::{self, MetaDb, orgs, projects};
use crate::serve;
use crate::test_support;

/// The version of the scenario and of this file's format. Frozen from the first release on: a
/// later change to what `build` writes is a new number, with its code path beside the old one.
pub(super) const GENERATOR: u64 = 1;

/// The key of every sealed value of the golden directory: the bytes `0, 1, …, 31`. A public test
/// key; it seals only fixture values (SF3).
pub(super) const GOLDEN_KEY: [u8; 32] = {
    let mut key = [0u8; 32];
    let mut index = 0;
    while index < 32 {
        key[index] = index as u8;
        index += 1;
    }
    key
};

/// The owner's password (signs in with it; the hash is an argon2 PHC string, not pinned).
pub(super) const OWNER_PASSWORD: &str = "golden-password-1";
/// The owner's ingest key, on `golden-p1` (`sk`, 32 tail characters, base62).
pub(super) const INGEST_KEY: &str = "sk-cetra-GoldenP1IngestKey0123456789abcde";
/// The public key of `golden-p1` (`pk`, 24 tail characters).
pub(super) const PUBLIC_KEY: &str = "pk-cetra-GoldenPublicP1Key0123456";
/// The owner's personal access token (`pat`, 32 tail characters).
pub(super) const PERSONAL_TOKEN: &str = "pat-cetra-GoldenOwnerToken0123456789012345";
/// The owner's web session cookie value (43 characters of the base64url alphabet).
pub(super) const SESSION_TOKEN: &str = "GoldenSessionTokenABCDEFGHIJKLMNOPQRSTUVWXY";
/// A pending invite's token, for the invite hash.
pub(super) const INVITE_TOKEN: &str = "GoldenInviteTokenABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Tables the startup tasks of `serve::bind` empty in the golden backup, with the reason. Empty
/// after the first build: every table keeps a row through a state the startup tasks leave alone
/// (see `build.rs`).
const EMPTY_TABLES: &[(&str, &str)] = &[];

/// The fresh golden directory, built once per test process on first use (SF6, the M8 pattern).
struct Fresh {
    /// Keeps the temp directory alive for the process.
    _dir: tempfile::TempDir,
    golden: PathBuf,
}

static FRESH: OnceLock<Fresh> = OnceLock::new();

/// The fresh golden directory. Never write into it: copy it first. Not inside a tokio runtime:
/// building it runs a server on a runtime of its own.
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

/// Runs `future` to completion on a runtime of its own. Not called from inside a runtime.
pub(super) fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for the golden directory")
        .block_on(future)
}

/// Writes the golden directory into `out` (which must not exist): the backup and the socket
/// lines (build.rs), then `expected.json`, computed from a restored copy of the backup.
pub(super) fn generate(out: &Path) {
    let lines = block_on(build::build(out));
    assert!(!lines.is_empty(), "the scenario made socket calls");
    let expected = block_on(async {
        let restored = restore_copy(out).await;
        let meta = MetaDb::open_existing(restored.path())
            .await
            .expect("the restored copy opens");
        let secrets = SecretBox::new(&GOLDEN_KEY);
        let dump = dump::dump(&meta, &secrets, restored.path()).await;
        meta.close().await;
        dump
    });
    let mut text = serde_json::to_string_pretty(&expected).expect("expected.json serializes");
    text.push('\n');
    fs::write(out.join("expected.json"), text).expect("write expected.json");
}

/// Copies `dir`'s backup files (`meta.db`, `catalog.db`, `secret.key`, `objects/` when present)
/// into a new temp data directory, as step 3 of docs/self-hosting/backup.md § Restore says. `dir`
/// is never written.
fn copy_files(dir: &Path) -> tempfile::TempDir {
    let data = tempfile::tempdir().expect("a temp data directory for the restore");
    for name in ["meta.db", "catalog.db", "secret.key"] {
        fs::copy(dir.join(name), data.path().join(name))
            .unwrap_or_else(|error| panic!("{}: copy {name}: {error}", basename(dir)));
    }
    let objects = dir.join("objects");
    if objects.is_dir() {
        copy_tree(&objects, &data.path().join("objects"));
    }
    data
}

/// Recursive copy of a directory of regular files.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create a copied directory");
    for entry in fs::read_dir(from).expect("read a directory to copy") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

/// Steps 5 and 6 of docs/self-hosting/backup.md § Restore on a directory `copy_files` made: the
/// orphan collection (which must find no missing object) and the migrations.
async fn restore_steps(data_dir: &Path) -> Result<(), AdminError> {
    let config = test_support::config(data_dir);
    let report = admin::gc_orphans(&config, Duration::from_secs(3600), false, false).await?;
    assert_eq!(
        report.missing,
        0,
        "{}: the restored catalog needs objects the backup does not have",
        basename(data_dir)
    );
    admin::migrate(&config).await
}

/// `copy_files`, then `restore_steps`, unwrapped.
async fn restore_copy(dir: &Path) -> tempfile::TempDir {
    let data = copy_files(dir);
    restore_steps(data.path())
        .await
        .unwrap_or_else(|error| panic!("{}: restore: {error}", basename(dir)));
    data
}

/// The last path component of `dir`, for assertion messages.
fn basename(dir: &Path) -> String {
    dir.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// The row count of every table of the `meta.db` at `path`, other than SQLite's own and the
/// migrations, read on a copy: a golden directory is never written.
pub(super) async fn row_counts(path: &Path) -> Vec<(String, i64)> {
    {
        let temp = tempfile::tempdir().expect("a temp dir for the copy");
        let copy = temp.path().join("meta.db");
        fs::copy(path, &copy).expect("copy meta.db");
        let mut connection = SqliteConnectOptions::new()
            .filename(&copy)
            .connect()
            .await
            .expect("open the copy of meta.db");
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' ORDER BY name",
        )
        .fetch_all(&mut connection)
        .await
        .expect("list the tables");
        let mut counts = Vec::new();
        for name in names {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM \"{name}\""
            )))
            .fetch_one(&mut connection)
            .await
            .expect("count a table");
            counts.push((name, count));
        }
        connection.close().await.ok();
        counts
    }
}

/// Writes one golden server directory, named by `CETRA_SERVER_GOLDEN_OUT` (absolute, not yet
/// existing). Its name is the workspace version, or `scratch*` for a look (R8.8).
#[test]
#[ignore = "writes a fixtures directory; run by just golden-dir <version>"]
fn write_server_golden_dir() {
    let out = PathBuf::from(
        std::env::var_os("CETRA_SERVER_GOLDEN_OUT")
            .expect("CETRA_SERVER_GOLDEN_OUT names the directory to write"),
    );
    assert!(
        out.is_absolute(),
        "CETRA_SERVER_GOLDEN_OUT is an absolute path"
    );
    assert!(!out.exists(), "CETRA_SERVER_GOLDEN_OUT must not exist yet");
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .expect("CETRA_SERVER_GOLDEN_OUT ends in a UTF-8 directory name");
    assert!(
        name.starts_with("scratch") || name == env!("CARGO_PKG_VERSION"),
        "{name}: the workspace is {}; a golden directory is named after the version it is written by (or scratch*)",
        env!("CARGO_PKG_VERSION")
    );
    fs::create_dir_all(out.parent().expect("CETRA_SERVER_GOLDEN_OUT has a parent"))
        .expect("create the parent of CETRA_SERVER_GOLDEN_OUT");
    generate(&out);
    let (files, bytes) = dir_size(&out);
    println!(
        "golden server directory {}: {files} files, {bytes} bytes",
        out.display()
    );
}

/// The number of regular files under `dir` and their total size.
fn dir_size(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).expect("read a directory") {
            let entry = entry.expect("a directory entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files += 1;
                bytes += entry.metadata().expect("file metadata").len();
            }
        }
    }
    (files, bytes)
}

#[test]
fn every_table_has_a_golden_row() {
    let counts = block_on(row_counts(&fresh_golden_dir().join("meta.db")));
    assert!(counts.len() > 40, "the schema has its tables: {counts:?}");
    for (table, count) in counts {
        if EMPTY_TABLES.iter().any(|(name, _)| *name == table) {
            continue;
        }
        assert!(
            count > 0,
            "table {table} has no row in the golden scenario: add one in golden_tests/build.rs"
        );
    }
}

/// The bound on the total size of a golden server directory (SF7): it grows the repository by its
/// size for every release. Do not raise it without the coordinator. 2 MiB since the merge with
/// tracks CA, V, AQ, AS and IC (coordinator): the schema's floor is one 4 KB page per B-tree
/// (66 tables and 153 indexes, about 950 KB), and the backup was 1.15 MB at that merge.
const GOLDEN_MAX_BYTES: u64 = 2 << 20;

/// The payload of the probe connection that step 8 of `check_dir` seals with the golden key.
const PROBE_SECRET: &[u8] = b"golden-probe-secret";

/// The admin fields of `manifest.json` (SF5, docs/self-hosting/backup.md § The manifest) and the
/// JSON types each may have.
const MANIFEST_KEYS: &[(&str, &[&str])] = &[
    ("format", &["number"]),
    ("cetra_version", &["string"]),
    ("created_at", &["string"]),
    ("mode", &["string"]),
    ("object_store", &["string"]),
    ("encryption_key", &["string"]),
    ("flushed_seq", &["object"]),
    ("gc_grace_secs", &["number"]),
    ("restorable_until", &["null", "string"]),
    ("files", &["array"]),
    ("objects", &["object"]),
    ("meta_format", &["number"]),
    ("meta_schema", &["number"]),
    ("storage_format", &["number"]),
    ("catalog_schema", &["number"]),
];

/// The golden server directories committed under `fixtures/server/golden/`, sorted. None before the
/// first release: the directory does not exist yet.
fn committed_dirs() -> Vec<PathBuf> {
    let root = server_golden_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("list {}: {error}", root.display()))
        .map(|entry| entry.expect("read a golden directory entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `fixtures/server/golden/`, the committed golden server directories.
fn server_golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/server/golden")
}

/// The versions listed in `fixtures/storage/golden/RELEASES` (D-R-30: one list for both kinds of
/// golden directory), oldest first. Comment (`#`) and blank lines are skipped.
fn released_versions() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/storage/golden/RELEASES");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// `expected.json` of `dir`, which must name a generator this build knows (step 1).
fn read_expected(dir: &Path) -> Value {
    let name = basename(dir);
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

/// The JSON type of `value`, as `MANIFEST_KEYS` names them.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The highest catalog migration of the storage crate: the leading number of its file names.
fn catalog_top_migration() -> i64 {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../storage/migrations/catalog");
    fs::read_dir(root)
        .unwrap_or_else(|error| panic!("list {root}: {error}"))
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            name.split('_').next()?.parse::<i64>().ok()
        })
        .max()
        .expect("the catalog has migrations")
}

/// `PRAGMA user_version` of the database copy at `path` (`meta.db` or `catalog.db`).
async fn user_version(path: &Path) -> i64 {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .unwrap_or_else(|error| panic!("open a copy of a database: {error}"));
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("read PRAGMA user_version: {error}"));
    connection
        .close()
        .await
        .unwrap_or_else(|error| panic!("close a copy of meta.db: {error}"));
    version
}

/// The `<file>.pre-m<N>` copies in `data`: exactly the one `expected` names, or none (D-N-22).
fn assert_pre_copies(name: &str, data: &Path, file: &str, expected: Option<i64>) {
    let prefix = format!("{file}.pre-m");
    let mut found: Vec<String> = fs::read_dir(data)
        .expect("list the restored directory")
        .map(|entry| {
            entry
                .expect("a restored directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|entry| entry.starts_with(&prefix))
        .collect();
    found.sort();
    let wanted: Vec<String> = expected
        .map(|version| format!("{file}.pre-m{version:04}"))
        .into_iter()
        .collect();
    assert_eq!(
        found, wanted,
        "{name}: step 4: the pre-migration copies of {file} after the restore"
    );
}

/// `manifest.json` of `dir`, parsed (step 2).
fn read_manifest(dir: &Path) -> Value {
    let name = basename(dir);
    let text = fs::read_to_string(dir.join("manifest.json"))
        .unwrap_or_else(|error| panic!("{name}: step 2: read manifest.json: {error}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{name}: step 2: manifest.json is not JSON: {error}"))
}

/// A field of an expected or manifest object as an `i64`, or a panic naming the step.
fn number(value: &Value, what: &str, name: &str) -> i64 {
    value
        .as_i64()
        .unwrap_or_else(|| panic!("{name}: {what} is not a number"))
}

/// Step 5: every table of `expected.json`'s `rows`, restored, read by column name in the order
/// `expected.json` names them, equals the expected rows. A missing table or column is a format
/// change (D-R-34). New tables and columns are ignored.
async fn check_rows(name: &str, data: &Path, expected: &Value) {
    let mut connection = SqliteConnectOptions::new()
        .filename(data.join("meta.db"))
        .connect()
        .await
        .unwrap_or_else(|error| panic!("{name}: step 5: open the restored meta.db: {error}"));
    let columns = expected["columns"]
        .as_object()
        .unwrap_or_else(|| panic!("{name}: step 5: expected.json has no columns"));
    for (table, wanted) in columns {
        let wanted: Vec<&str> = wanted
            .as_array()
            .expect("the columns of a table are a list")
            .iter()
            .map(|column| column.as_str().expect("a column name is text"))
            .collect();
        let have: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA table_info(\"{table}\")"
        )))
        .fetch_all(&mut connection)
        .await
        .unwrap_or_else(|error| panic!("{name}: step 5: read the columns of {table}: {error}"))
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
        assert!(
            !have.is_empty(),
            "{name}: step 5: table {table} of the golden directory is gone: a format change (D-R-34)"
        );
        for column in &wanted {
            assert!(
                have.iter().any(|have_name| have_name.as_str() == *column),
                "{name}: step 5: table {table} column {column} of the golden directory is gone: a format change (D-R-34)"
            );
        }
        let list: Vec<String> = wanted
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect();
        let order: Vec<String> = (1..=wanted.len()).map(|n| n.to_string()).collect();
        let sql = format!(
            "SELECT {} FROM \"{table}\" ORDER BY {}",
            list.join(", "),
            order.join(", ")
        );
        let fetched = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&mut connection)
            .await
            .unwrap_or_else(|error| panic!("{name}: step 5: read the rows of {table}: {error}"));
        let rows: Vec<Value> = fetched
            .iter()
            .map(|row| {
                Value::Array(
                    (0..wanted.len())
                        .map(|index| dump::cell(row, index))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            Value::Array(rows),
            expected["rows"][table.as_str()],
            "{name}: step 5: the rows of {table} differ from expected.json"
        );
    }
    connection.close().await.ok();
}

/// Step 8's writes on the restored copy, with the server stopped: a project and an LLM connection
/// whose key is sealed with the golden key.
async fn probe_writes(data: &Path) {
    let meta = MetaDb::open_existing(data)
        .await
        .expect("open the restored copy for the probe writes");
    let org = orgs::org_by_slug(meta.reader(), "golden-org")
        .await
        .expect("read the golden org")
        .expect("the golden org exists");
    let org_id = org.id;
    let connection_id = Uuid::now_v7();
    let secret_enc = SecretBox::new(&GOLDEN_KEY)
        .seal(
            PROBE_SECRET,
            Aad {
                table: "llm_connections",
                column: "secret_enc",
                row_id: &connection_id,
            },
        )
        .expect("seal the probe secret");
    let now = Clock::System.now_ns();
    meta.write(move |conn| {
        Box::pin(async move {
            projects::insert_project(conn, org_id, "Golden Probe", "golden-probe", None, now)
                .await?;
            connections::insert_connection(
                conn,
                connection_id,
                org_id,
                ConnectionFields {
                    name: "golden-probe".to_owned(),
                    adapter: "openai".to_owned(),
                    base_url: None,
                    secret_enc,
                    api_key_hint: String::new(),
                    extra_headers_enc: None,
                    extra_header_names: Vec::new(),
                    default_model: None,
                    structured_output: "native".to_owned(),
                    max_concurrency: 4,
                    requests_per_minute: 300,
                },
                None,
                now,
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("write the probe rows");
    meta.close().await;
}

/// Checks the golden server directory `dir` against the current build (SF7, D-R-38). `dir` is
/// never written: restores run on copies, and the server runs on a restored copy only.
async fn check_dir(dir: &Path) {
    let started = Instant::now();
    let name = basename(dir);

    // 1. expected.json.
    let expected = read_expected(dir);
    let meta_format = number(
        &expected["meta_format"],
        "step 1: expected.meta_format",
        &name,
    );
    let meta_schema = number(
        &expected["meta_schema"],
        "step 1: expected.meta_schema",
        &name,
    );

    // 2. The manifest: format 1, every key with its JSON type, every listed file's digest.
    let manifest = read_manifest(dir);
    assert_eq!(
        manifest["format"],
        json!(1),
        "{name}: step 2: manifest.json format"
    );
    for (key, types) in MANIFEST_KEYS {
        let value = manifest
            .get(*key)
            .unwrap_or_else(|| panic!("{name}: step 2: manifest.json has no {key}"));
        assert!(
            types.contains(&json_type(value)),
            "{name}: step 2: manifest.json {key} is a {}, not one of {types:?}",
            json_type(value)
        );
    }
    for file in manifest["files"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: step 2: manifest.json files is not a list"))
    {
        let path = file["path"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: step 2: a files entry has no path"));
        let bytes = fs::read(dir.join(path))
            .unwrap_or_else(|error| panic!("{name}: step 2: read {path}: {error}"));
        assert_eq!(
            file["sha256"].as_str(),
            Some(hex::encode(Sha256::digest(&bytes)).as_str()),
            "{name}: step 2: the sha256 of {path} differs from manifest.json"
        );
    }
    assert_eq!(
        manifest["meta_format"].as_i64(),
        Some(meta_format),
        "{name}: step 2: manifest.json meta_format differs from expected.json"
    );

    // 3. The markers, read on a copy: a connection may switch the journal mode of its file.
    let markers = copy_files(dir);
    assert_eq!(
        user_version(&markers.path().join("meta.db")).await,
        meta_format,
        "{name}: step 3: PRAGMA user_version of meta.db"
    );
    let storage_format = number(&manifest["storage_format"], "step 2: storage_format", &name);
    assert_eq!(
        user_version(&markers.path().join("catalog.db")).await,
        storage_format,
        "{name}: step 3: PRAGMA user_version of catalog.db"
    );
    drop(markers);

    // 4. The restore of § Restore (steps 3, 5 and 6): the pre-migration copies it leaves.
    let restored = restore_copy(dir).await;
    let data = restored.path();
    let catalog_schema = number(&manifest["catalog_schema"], "step 2: catalog_schema", &name);
    assert_pre_copies(
        &name,
        data,
        "meta.db",
        (meta_schema < meta::expected_version()).then_some(meta_schema),
    );
    assert_pre_copies(
        &name,
        data,
        "catalog.db",
        (catalog_schema < catalog_top_migration()).then_some(catalog_schema),
    );

    // 5. The rows of meta.db by column name, before the server opens the copy.
    check_rows(&name, data, &expected).await;

    // 6. The reads, the secrets, the sign-ins and the encryption check. `expected.json` was
    // computed from a restored copy as well, before any server ran on it.
    let loaded = crypto::load_key(None, data, true)
        .unwrap_or_else(|error| panic!("{name}: step 6: load secret.key: {error}"));
    assert!(
        loaded.key == GOLDEN_KEY,
        "{name}: step 6: secret.key is not the golden key"
    );
    let secrets = SecretBox::new(&GOLDEN_KEY);
    let meta = MetaDb::open_existing(data)
        .await
        .unwrap_or_else(|error| panic!("{name}: step 6: open the restored meta.db: {error}"));
    let outcome = crypto::verify_or_record(&meta, &secrets, &crypto::key_path(data))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 6: the encryption check: {error}"));
    assert_eq!(
        outcome,
        CheckOutcome::Matched,
        "{name}: step 6: the encryption check"
    );
    let dumped = dump::dump(&meta, &secrets, data).await;
    meta.close().await;
    for key in ["reads", "secrets", "auth", "encryption_check"] {
        assert_eq!(
            dumped[key], expected[key],
            "{name}: step 6: {key} differs from expected.json"
        );
    }

    // 7. It serves: /readyz answers 200.
    let running = serve::bind(test_support::config(data), None)
        .await
        .unwrap_or_else(|error| panic!("{name}: step 7: bind the restored directory: {error}"));
    let ready = reqwest::get(format!("http://{}/readyz", running.local_addr))
        .await
        .unwrap_or_else(|error| panic!("{name}: step 7: GET /readyz: {error}"));
    assert_eq!(
        ready.status(),
        reqwest::StatusCode::OK,
        "{name}: step 7: /readyz"
    );
    running
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{name}: step 7: shut the server down: {error}"));

    // 8. New writes on the old directory, stopped: a project and a connection sealed with the
    // golden key, then a key rotation (D-I-3), then every secret, the probe's too, opens with the
    // new key to the same plaintext.
    probe_writes(data).await;
    admin::rotate_encryption_key(&test_support::config(data), None)
        .await
        .unwrap_or_else(|error| panic!("{name}: step 8: rotate the encryption key: {error}"));
    let rotated = crypto::load_key(None, data, true)
        .unwrap_or_else(|error| panic!("{name}: step 8: load the rotated key: {error}"));
    assert_ne!(
        rotated.key, GOLDEN_KEY,
        "{name}: step 8: the rotation kept the golden key"
    );
    let after = dump::secret_values(data, &SecretBox::new(&rotated.key)).await;
    for (key, value) in expected["secrets"].as_object().expect("expected secrets") {
        assert_eq!(
            after.get(key),
            Some(value),
            "{name}: step 8: {key} does not open to its plaintext after the rotation"
        );
    }
    assert_eq!(
        after.get("llm_connections.secret_enc.golden-probe"),
        Some(&json!(String::from_utf8_lossy(PROBE_SECRET))),
        "{name}: step 8: the probe's secret after the rotation"
    );

    // 9. The recorded socket lines, in order, on a server of the restored directory. Each backup
    // goes to one fresh path outside the data directory: the first finds it absent, the second
    // finds it non-empty, as recorded.
    let lines = socket_lines(dir, &name);
    let target_root = tempfile::tempdir().expect("a temp dir for the backup target");
    let target = target_root
        .path()
        .join("out")
        .to_string_lossy()
        .into_owned();
    let running = serve::bind(test_support::config(data), None)
        .await
        .unwrap_or_else(|error| panic!("{name}: step 9: bind the restored directory: {error}"));
    for line in &lines {
        let mut request = line["request"].clone();
        if let Some(to) = request.get_mut("to")
            && to.as_str() == Some(build::TARGET)
        {
            *to = json!(target);
        }
        let answer = build::exchange(data, &request.to_string()).await;
        let response: Value = serde_json::from_str(&answer)
            .unwrap_or_else(|error| panic!("{name}: step 9: the answer is not JSON: {error}"));
        let recorded = &line["response"];
        assert_eq!(
            response["ok"], recorded["ok"],
            "{name}: step 9: ok of {request}"
        );
        assert_eq!(
            response.get("code"),
            recorded.get("code"),
            "{name}: step 9: code of {request}"
        );
        decode_response(&recorded.to_string())
            .unwrap_or_else(|error| panic!("{name}: step 9: a recorded answer: {error}"));
    }
    running
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{name}: step 9: shut the server down: {error}"));
    eprintln!("check_dir {name}: {:?}", started.elapsed());
}

/// The recorded `{request, response}` lines of `admin_socket.jsonl` in `dir`.
fn socket_lines(dir: &Path, name: &str) -> Vec<Value> {
    fs::read_to_string(dir.join("admin_socket.jsonl"))
        .unwrap_or_else(|error| panic!("{name}: step 9: read admin_socket.jsonl: {error}"))
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("{name}: step 9: a socket line is not JSON: {error}")
            })
        })
        .collect()
}

/// `PRAGMA user_version` of the `meta.db` at `path`, set by a connection that sets no journal
/// mode: the same options as `MetaDb`'s own check (`check_file`), so the file keeps its mode.
async fn stamp_meta_version(path: &Path, version: i64) {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .disable_statement_logging()
        .connect()
        .await
        .unwrap_or_else(|error| panic!("open a copy of meta.db: {error}"));
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
        .unwrap_or_else(|error| panic!("close a copy of meta.db: {error}"));
}

/// Inserts an applied migration this build does not have into the `meta.db` copy at `path`.
async fn insert_unknown_migration(path: &Path, version: i64) {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .disable_statement_logging()
        .connect()
        .await
        .unwrap_or_else(|error| panic!("open a copy of meta.db: {error}"));
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES (?, 'future', 1, ?, 0)",
    )
    .bind(version)
    .bind(Vec::<u8>::new())
    .execute(&mut connection)
    .await
    .unwrap_or_else(|error| panic!("insert a migration row: {error}"));
    connection
        .close()
        .await
        .unwrap_or_else(|error| panic!("close a copy of meta.db: {error}"));
}

/// The directories the release-rule and restore tests run on: the committed ones, or the fresh one
/// when none is committed yet.
fn committed_or_fresh() -> Vec<PathBuf> {
    let mut dirs = committed_dirs();
    if dirs.is_empty() {
        dirs.push(fresh_golden_dir().to_path_buf());
    }
    dirs
}

#[test]
fn a_fresh_golden_backup_restores() {
    let dir = fresh_golden_dir();
    block_on(check_dir(dir));
}

#[test]
fn the_committed_golden_backups_restore() {
    let dirs = committed_dirs();
    if dirs.is_empty() {
        eprintln!(
            "the_committed_golden_backups_restore: no server golden directory is committed yet"
        );
        return;
    }
    for dir in dirs {
        block_on(check_dir(&dir));
    }
}

#[test]
fn golden_backups_stay_small() {
    let mut dirs = committed_dirs();
    dirs.push(fresh_golden_dir().to_path_buf());
    for dir in dirs {
        let name = basename(&dir);
        let (files, bytes) = dir_size(&dir);
        eprintln!("golden_backups_stay_small: {name}: {files} files, {bytes} bytes");
        assert!(
            bytes <= GOLDEN_MAX_BYTES,
            "{name}: {bytes} bytes, over {GOLDEN_MAX_BYTES}: a golden directory grows the repository by its size for every release: shrink the scenario in golden_tests/build.rs; do not raise the limit without the coordinator"
        );
    }
}

#[test]
fn a_newer_golden_backup_is_refused() {
    for dir in committed_or_fresh() {
        let name = basename(&dir);

        // A newer format: refused by the preflight, before anything runs.
        let copy = copy_files(&dir);
        block_on(stamp_meta_version(
            &copy.path().join("meta.db"),
            i64::from(meta::META_FORMAT_VERSION) + 1,
        ));
        let digest = dir_digest(copy.path());
        let error = block_on(restore_steps(copy.path()))
            .expect_err(&format!("{name}: a newer format is refused"));
        assert!(
            error.to_string().contains("format version"),
            "{name}: a newer format is refused with its version: {error}"
        );
        assert_eq!(
            dir_digest(copy.path()),
            digest,
            "{name}: a refused restore changed the directory"
        );

        // A newer schema: an applied migration this build does not have.
        let copy = copy_files(&dir);
        block_on(insert_unknown_migration(
            &copy.path().join("meta.db"),
            meta::expected_version() + 1,
        ));
        let digest = dir_digest(copy.path());
        let error = block_on(restore_steps(copy.path()))
            .expect_err(&format!("{name}: a newer schema is refused"));
        assert!(
            error.to_string().contains("applied migration"),
            "{name}: a newer schema is refused with its migration: {error}"
        );
        assert_eq!(
            dir_digest(copy.path()),
            digest,
            "{name}: a refused restore changed the directory"
        );
    }
}

#[test]
fn every_released_version_has_a_server_golden_directory() {
    for version in released_versions() {
        let dir = server_golden_root().join(&version);
        assert!(
            dir.is_dir(),
            "RELEASES lists {version}, which has no server golden directory {}",
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
fn every_server_golden_directory_is_released() {
    let released = released_versions();
    for dir in committed_dirs() {
        let name = basename(&dir);
        assert!(
            released.contains(&name),
            "{name}: a golden directory that RELEASES does not list (a scratch* directory: delete it)"
        );
    }
}
