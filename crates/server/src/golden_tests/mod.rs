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
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

use crate::admin::{self, AdminError};
use crate::crypto::SecretBox;
use crate::meta::MetaDb;
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
pub(super) const INGEST_KEY: &str = "sk-llmobs-GoldenP1IngestKey0123456789abcde";
/// The public key of `golden-p1` (`pk`, 24 tail characters).
pub(super) const PUBLIC_KEY: &str = "pk-llmobs-GoldenPublicP1Key0123456";
/// The owner's personal access token (`pat`, 32 tail characters).
pub(super) const PERSONAL_TOKEN: &str = "pat-llmobs-GoldenOwnerToken0123456789012345";
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

/// Writes one golden server directory, named by `LLMOBS_SERVER_GOLDEN_OUT` (absolute, not yet
/// existing). Its name is the workspace version, or `scratch*` for a look (R8.8).
#[test]
#[ignore = "writes a fixtures directory; run by just golden-dir <version>"]
fn write_server_golden_dir() {
    let out = PathBuf::from(
        std::env::var_os("LLMOBS_SERVER_GOLDEN_OUT")
            .expect("LLMOBS_SERVER_GOLDEN_OUT names the directory to write"),
    );
    assert!(
        out.is_absolute(),
        "LLMOBS_SERVER_GOLDEN_OUT is an absolute path"
    );
    assert!(!out.exists(), "LLMOBS_SERVER_GOLDEN_OUT must not exist yet");
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .expect("LLMOBS_SERVER_GOLDEN_OUT ends in a UTF-8 directory name");
    assert!(
        name.starts_with("scratch") || name == env!("CARGO_PKG_VERSION"),
        "{name}: the workspace is {}; a golden directory is named after the version it is written by (or scratch*)",
        env!("CARGO_PKG_VERSION")
    );
    fs::create_dir_all(out.parent().expect("LLMOBS_SERVER_GOLDEN_OUT has a parent"))
        .expect("create the parent of LLMOBS_SERVER_GOLDEN_OUT");
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
