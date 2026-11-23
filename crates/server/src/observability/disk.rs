//! What the data directory holds, read from disk without the storage engine (track TL § 4, D-TL-6): file sizes, the format markers of `meta.db`, `catalog.db` and the WAL segments, and the catalog's live files per table and level. The debug bundle reads it when no server answers on `admin.sock`. The server's `debug_info` reads the catalog counts. The SQLite connections are read-only, and immutable when no server runs, so reading writes no file, not even `-shm`.

use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions as _, Connection as _};

/// Live data files of one table at one level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelFiles {
    /// The table: `spans` or `scores`.
    pub table: String,
    /// The LSM level of the files.
    pub level: i64,
    /// Live files at this level.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
}

/// The first bytes of a WAL segment (spec 03 § 1, `cetra_storage::wal::segment`): the magic,
/// then the format version as a little-endian `u16`.
const WAL_SEGMENT_MAGIC: [u8; 4] = *b"LWAL";
/// Live files per table and level (`crates/storage/src/catalog/schema.txt`).
const CATALOG_LEVELS_SQL: &str = "SELECT table_name, level, COUNT(*), COALESCE(SUM(size_bytes), 0) FROM data_files WHERE state = 'active' GROUP BY table_name, level ORDER BY table_name, level";

/// One SQLite file: its size, its `-wal` sidecar and its format markers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DbFileFacts {
    /// The file exists.
    pub(crate) present: bool,
    /// The file's size, 0 when absent.
    pub(crate) bytes: u64,
    /// Size of the `-wal` file beside it (0 when absent).
    pub(crate) wal_bytes: u64,
    /// The versions below were read.
    pub(crate) readable: bool,
    /// `PRAGMA user_version` (the format version).
    pub(crate) format: Option<i64>,
    /// The highest `_sqlx_migrations.version`.
    pub(crate) schema: Option<i64>,
}

/// One WAL directory (`wal/spans` or `wal/scores`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WalDirFacts {
    /// The directory exists.
    pub(crate) present: bool,
    /// `*.wal` files.
    pub(crate) segments: u64,
    /// Their total size.
    pub(crate) bytes: u64,
    /// Distinct header format versions, sorted.
    pub(crate) header_versions: Vec<u16>,
    /// Segments shorter than 6 bytes or without the `LWAL` magic.
    pub(crate) bad_headers: u64,
}

/// The two WAL directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WalFacts {
    /// `wal/spans`.
    pub(crate) spans: WalDirFacts,
    /// `wal/scores`.
    pub(crate) scores: WalDirFacts,
}

/// Everything the data directory tells without the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DiskFacts {
    /// The data directory exists.
    pub(crate) data_dir_present: bool,
    /// `meta.db`.
    pub(crate) meta_db: DbFileFacts,
    /// `catalog.db`.
    pub(crate) catalog_db: DbFileFacts,
    /// `None` when `catalog.db` is absent or unreadable.
    pub(crate) catalog_files: Option<Vec<LevelFiles>>,
    /// The WAL directories.
    pub(crate) wal: WalFacts,
}

/// `immutable` is for a directory no server has open: SQLite then takes no lock and reads no
/// `-wal`. With a running server, pass `false`: a WAL reader sees the committed state and blocks
/// no writer.
pub(crate) async fn catalog_levels(
    path: &Path,
    immutable: bool,
) -> Result<Vec<LevelFiles>, sqlx::Error> {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(immutable)
        .create_if_missing(false)
        .disable_statement_logging()
        .connect()
        .await?;
    let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(CATALOG_LEVELS_SQL)
        .fetch_all(&mut connection)
        .await;
    // Closing a read-only connection cannot lose data; its error is not interesting.
    let _ = connection.close().await;
    Ok(rows?
        .into_iter()
        .map(|(table, level, files, bytes)| LevelFiles {
            table,
            level,
            files: u64::try_from(files).unwrap_or(0),
            bytes: u64::try_from(bytes).unwrap_or(0),
        })
        .collect())
}

/// `PRAGMA user_version` and the highest migration version of one SQLite file, read on an
/// immutable read-only connection that writes nothing.
async fn database_versions(path: &Path) -> Result<(i64, i64), sqlx::Error> {
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true)
        .create_if_missing(false)
        .disable_statement_logging()
        .connect()
        .await?;
    let versions = async {
        let format: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await?;
        let schema: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&mut connection)
                .await?;
        Ok::<_, sqlx::Error>((format, schema))
    }
    .await;
    // Closing a read-only connection cannot lose data; its error is not interesting.
    let _ = connection.close().await;
    versions
}

/// Size and format markers of one SQLite file and its `-wal` sidecar.
async fn db_file_facts(path: &Path) -> DbFileFacts {
    let present = path.is_file();
    let bytes = file_len(path);
    let wal_bytes = file_len(&sidecar(path, "-wal"));
    if !present {
        return DbFileFacts {
            present: false,
            bytes: 0,
            wal_bytes,
            readable: false,
            format: None,
            schema: None,
        };
    }
    match database_versions(path).await {
        Ok((format, schema)) => DbFileFacts {
            present,
            bytes,
            wal_bytes,
            readable: true,
            format: Some(format),
            schema: Some(schema),
        },
        Err(_) => DbFileFacts {
            present,
            bytes,
            wal_bytes,
            readable: false,
            format: None,
            schema: None,
        },
    }
}

/// Count and size the `*.wal` segments of one directory, and classify their headers.
fn wal_dir_facts(dir: &Path) -> WalDirFacts {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return WalDirFacts {
            present: false,
            segments: 0,
            bytes: 0,
            header_versions: Vec::new(),
            bad_headers: 0,
        };
    };
    let mut segments = 0_u64;
    let mut bytes = 0_u64;
    let mut bad_headers = 0_u64;
    let mut versions = BTreeSet::new();
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".wal"))
        {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        segments += 1;
        bytes = bytes.saturating_add(metadata.len());
        match read_header(&path) {
            Some(header) if header[..4] == WAL_SEGMENT_MAGIC => {
                versions.insert(u16::from_le_bytes([header[4], header[5]]));
            }
            _ => bad_headers += 1,
        }
    }
    WalDirFacts {
        present: true,
        segments,
        bytes,
        header_versions: versions.into_iter().collect(),
        bad_headers,
    }
}

/// The first 6 bytes of a file, `None` when shorter or unreadable.
fn read_header(path: &Path) -> Option<[u8; 6]> {
    let mut header = [0_u8; 6];
    let mut file = File::open(path).ok()?;
    file.read_exact(&mut header).ok()?;
    Some(header)
}

/// The size of a regular file, 0 when absent or not a file.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| {
            if metadata.is_file() {
                metadata.len()
            } else {
                0
            }
        })
        .unwrap_or(0)
}

/// `path` with `suffix` appended to its file name (`meta.db` → `meta.db-wal`).
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether a process holds `<data_dir>/LOCK`: `cetra serve` holds an exclusive `flock` on it
/// for as long as its storage is open (`cetra_storage`'s `lock_dir`). Opens the file
/// read-only, never creating it, and tries a shared lock without waiting; a lock it gets is
/// released when the handle drops at the end of this function. Absent or unreadable: not
/// held. While the shared lock is held (microseconds), a `cetra serve` starting at that very
/// instant fails its exclusive `try_lock` with "another cetra is running"; accepted: the
/// bundle runs once, by hand.
pub(crate) fn lock_held(data_dir: &Path) -> bool {
    let Ok(file) = File::open(data_dir.join("LOCK")) else {
        return false;
    };
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}

/// Read the data directory's facts from disk. Never opens the storage engine and writes nothing.
pub(crate) async fn disk_facts(data_dir: &Path) -> DiskFacts {
    let catalog = data_dir.join("catalog.db");
    let catalog_files = if catalog.is_file() {
        catalog_levels(&catalog, true).await.ok()
    } else {
        None
    };
    DiskFacts {
        data_dir_present: data_dir.is_dir(),
        meta_db: db_file_facts(&data_dir.join("meta.db")).await,
        catalog_db: db_file_facts(&catalog).await,
        catalog_files,
        wal: WalFacts {
            spans: wal_dir_facts(&data_dir.join("wal").join("spans")),
            scores: wal_dir_facts(&data_dir.join("wal").join("scores")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file under `dir`, recursive, as a relative path and a size, sorted.
    fn listing(dir: &Path) -> Vec<(String, u64)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if let Ok(metadata) = entry.metadata() {
                    let relative = path.strip_prefix(root).unwrap_or(path.as_path());
                    out.push((relative.display().to_string(), metadata.len()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[tokio::test]
    async fn a_stopped_server_s_directory_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let running = crate::serve::bind(crate::test_support::config(data), None)
            .await
            .unwrap();
        running.shutdown().await.unwrap();

        let facts = disk_facts(data).await;
        assert!(facts.data_dir_present);
        assert!(facts.meta_db.present && facts.meta_db.readable);
        assert_eq!(
            facts.meta_db.format,
            Some(i64::from(crate::meta::META_FORMAT_VERSION))
        );
        assert_eq!(facts.meta_db.schema, Some(crate::meta::expected_version()));
        assert!(facts.catalog_db.readable);
        assert_eq!(facts.catalog_db.format, Some(1));
        assert_eq!(facts.catalog_files, Some(vec![]));
        for wal in [&facts.wal.spans, &facts.wal.scores] {
            assert!(wal.present);
            assert_eq!(wal.bad_headers, 0);
            assert!(wal.header_versions.is_empty() || wal.header_versions == [1]);
        }
    }

    #[tokio::test]
    async fn reading_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let running = crate::serve::bind(crate::test_support::config(data), None)
            .await
            .unwrap();
        running.shutdown().await.unwrap();

        let before = listing(data);
        let _ = disk_facts(data).await;
        assert_eq!(listing(data), before);
    }

    #[tokio::test]
    async fn a_missing_directory_has_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let facts = disk_facts(&dir.path().join("absent")).await;
        assert!(!facts.data_dir_present);
        assert!(!facts.meta_db.present);
        assert!(!facts.catalog_db.present);
        assert_eq!(facts.catalog_files, None);
        assert!(!facts.wal.spans.present);
        assert!(!facts.wal.scores.present);
    }

    #[test]
    fn wal_headers_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        let spans = dir.path().join("spans");
        std::fs::create_dir(&spans).unwrap();
        let files: [(&str, &[u8]); 4] = [
            ("00000000000000000001.wal", b"LWAL\x01\x00\x00\x00"),
            ("00000000000000000002.wal", b"LWAL\x02\x00\x00\x00rest"),
            ("00000000000000000003.wal", b"LW"),
            ("00000000000000000004.wal", b"XXXXXXXX"),
        ];
        let mut total = 0_u64;
        for (name, content) in files {
            std::fs::write(spans.join(name), content).unwrap();
            total += content.len() as u64;
        }
        std::fs::write(spans.join("notes.txt"), b"LWAL\x09\x00").unwrap();

        let facts = wal_dir_facts(&spans);
        assert!(facts.present);
        assert_eq!(facts.segments, 4);
        assert_eq!(facts.header_versions, vec![1, 2]);
        assert_eq!(facts.bad_headers, 2);
        assert_eq!(facts.bytes, total);
    }

    #[tokio::test]
    async fn catalog_levels_groups_live_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let mut connection = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE data_files (table_name TEXT NOT NULL, level INTEGER NOT NULL, size_bytes INTEGER NOT NULL, state TEXT NOT NULL)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        let rows: [(&str, i64, i64, &str); 5] = [
            ("spans", 0, 10, "active"),
            ("spans", 0, 5, "active"),
            ("spans", 1, 100, "active"),
            ("scores", 0, 7, "active"),
            ("spans", 0, 999, "obsolete"),
        ];
        for (table, level, size, state) in rows {
            sqlx::query("INSERT INTO data_files VALUES (?, ?, ?, ?)")
                .bind(table)
                .bind(level)
                .bind(size)
                .bind(state)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        connection.close().await.unwrap();

        let levels = catalog_levels(&path, true).await.unwrap();
        assert_eq!(
            levels,
            vec![
                LevelFiles {
                    table: "scores".into(),
                    level: 0,
                    files: 1,
                    bytes: 7,
                },
                LevelFiles {
                    table: "spans".into(),
                    level: 0,
                    files: 2,
                    bytes: 15,
                },
                LevelFiles {
                    table: "spans".into(),
                    level: 1,
                    files: 1,
                    bytes: 100,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_corrupt_database_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        std::fs::write(&path, [b'x'; 100]).unwrap();
        let facts = db_file_facts(&path).await;
        assert!(facts.present);
        assert!(!facts.readable);
        assert_eq!(facts.format, None);
    }

    #[tokio::test]
    async fn the_lock_is_seen_while_a_server_runs() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let running = crate::serve::bind(crate::test_support::config(data), None)
            .await
            .unwrap();
        assert!(lock_held(data));
        running.shutdown().await.unwrap();
        // The storage releases its lock at shutdown; the file may stay.
        assert!(!lock_held(data));
    }

    #[test]
    fn a_missing_lock_file_is_not_held_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!lock_held(dir.path()));
        assert!(!dir.path().join("LOCK").exists());
    }

    #[test]
    fn level_files_round_trip() {
        let level = LevelFiles {
            table: "spans".into(),
            level: 2,
            files: 3,
            bytes: 4096,
        };
        let json = serde_json::to_string(&level).unwrap();
        let back: LevelFiles = serde_json::from_str(&json).unwrap();
        assert_eq!(back, level);
    }
}
