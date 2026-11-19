//! `catalog.db`: one connection, the M2 schema, and the load into [`Catalog`].
//!
//! `Storage::open` opens it and loads the in-memory [`Catalog`].
//!
//! A migration that has been in a release is never edited; a change is a new migration (spec 03 § 12).
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use cetra_core::ids::ProjectId;
use chrono::NaiveDate;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Row, SqliteConnection};
use uuid::Uuid;

use super::{Catalog, FileId, FileMeta, FileTable, insert_sorted};
use crate::deletion::{PendingDeletion, Tombstones};
use crate::error::CatalogError;
use crate::format::FORMAT_VERSION;
use crate::projects::ProjectStates;

/// `flushed_seq` for the two tables. A missing `table_state` row is `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableState {
    /// Spans.
    pub spans: u64,
    /// Scores.
    pub scores: u64,
}

/// Open `<data_dir>/catalog.db`, set the durability pragmas, and migrate.
///
/// The directory must already exist. Pragmas are applied on connect, before
/// the migrations run. This is the only connection: readers use the in-memory
/// catalog.
///
/// The format marker (`PRAGMA user_version`, spec 03 § 12) is checked before
/// anything is copied or migrated, and stamped once the catalog is current.
pub(crate) async fn open_catalog_db(data_dir: &Path) -> Result<SqliteConnection, CatalogError> {
    check_catalog(data_dir).await?;
    let path = data_dir.join("catalog.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(5000));
    let mut connection = options
        .connect()
        .await
        .map_err(|source| CatalogError::Open { path, source })?;
    let marker = read_user_version(&mut connection).await?;
    let applied = applied_version(&mut connection).await?;
    let expected = sqlx::migrate!("./migrations/catalog")
        .migrations
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0);
    pre_migration_copy(&mut connection, data_dir, "catalog.db", applied, expected).await?;
    sqlx::migrate!("./migrations/catalog")
        .run(&mut connection)
        .await
        .map_err(|source| match source {
            sqlx::migrate::MigrateError::VersionMissing(version) => {
                CatalogError::SchemaTooNew { version }
            }
            source => CatalogError::Migrate { source },
        })?;
    if marker == 0 {
        // The catalog is new or empty. A crash between the migrations and this
        // statement leaves a marker of 0 that the next open accepts.
        // `FORMAT_VERSION` is a constant integer, not input.
        sqlx::query(AssertSqlSafe(format!(
            "PRAGMA user_version = {FORMAT_VERSION}"
        )))
        .execute(&mut connection)
        .await
        .map_err(CatalogError::db)?;
    }
    Ok(connection)
}

/// Refuses a `catalog.db` this build does not read (spec 03 § 12) on a
/// read-only connection, so a refused directory comes back unchanged:
/// connecting for writing with `journal_mode=WAL` rewrites the header of a
/// rollback-journal file, and closing a writing connection checkpoints.
/// `Storage::open` calls it before anything in the directory is written; a
/// missing `catalog.db` is a new directory.
pub(crate) async fn check_catalog(data_dir: &Path) -> Result<(), CatalogError> {
    let path = data_dir.join("catalog.db");
    if !path.is_file() {
        return Ok(());
    }
    let mut reader = SqliteConnectOptions::new()
        .filename(&path)
        .read_only(true)
        .connect()
        .await
        .map_err(|source| CatalogError::Open {
            path: path.clone(),
            source,
        })?;
    let checked = check_format(&mut reader).await;
    reader.close().await.map_err(CatalogError::db)?;
    checked
}

/// Refuses a directory this build does not read (spec 03 § 12). Nothing is
/// copied, migrated or stamped before this check passes.
async fn check_format(connection: &mut SqliteConnection) -> Result<(), CatalogError> {
    let marker = read_user_version(connection).await?;
    // No older format exists, so any other value (a newer one, or a corrupt
    // negative one) is refused.
    if marker != 0 && marker != i64::from(FORMAT_VERSION) {
        return Err(CatalogError::FormatTooNew {
            found: marker,
            supported: FORMAT_VERSION,
        });
    }
    let applied = applied_version(connection).await?;
    if marker == 0 && applied > 0 && catalog_has_data(connection).await? {
        return Err(CatalogError::PreFormat);
    }
    if let Some(version) = unknown_migration(connection).await? {
        return Err(CatalogError::SchemaTooNew { version });
    }
    Ok(())
}

/// `PRAGMA user_version`: the data directory's format marker, `0` when unset.
async fn read_user_version(connection: &mut SqliteConnection) -> Result<i64, CatalogError> {
    sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(CatalogError::db)
}

/// Whether a table that holds data has a row, among the tables an older
/// catalog may have. Tables that do not exist yet (migrations are pending) are
/// skipped, and `table_state` counts only a flushed sequence above `0`.
async fn catalog_has_data(connection: &mut SqliteConnection) -> Result<bool, CatalogError> {
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&mut *connection)
            .await
            .map_err(CatalogError::db)?;
    let present = |name: &str| tables.iter().any(|table| table == name);
    // Only these constant names are interpolated into the SQL below.
    for table in [
        "data_files",
        "deletions",
        "media",
        "projects",
        "retention_watermarks",
    ] {
        if present(table) {
            let found: bool = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM {table})"
            )))
            .fetch_one(&mut *connection)
            .await
            .map_err(CatalogError::db)?;
            if found {
                return Ok(true);
            }
        }
    }
    if present("table_state") {
        let found: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM table_state WHERE flushed_seq > 0)")
                .fetch_one(&mut *connection)
                .await
                .map_err(CatalogError::db)?;
        return Ok(found);
    }
    Ok(false)
}

/// The first applied migration this build does not have, read without
/// running the migrator (which would write).
async fn unknown_migration(connection: &mut SqliteConnection) -> Result<Option<i64>, CatalogError> {
    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await
    .map_err(CatalogError::db)?;
    if present.is_none() {
        return Ok(None);
    }
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *connection)
            .await
            .map_err(CatalogError::db)?;
    let known = sqlx::migrate!("./migrations/catalog").migrations;
    Ok(applied
        .into_iter()
        .find(|version| !known.iter().any(|migration| migration.version == *version)))
}

/// The highest applied migration version, or `0` when the database has none.
async fn applied_version(connection: &mut SqliteConnection) -> Result<i64, CatalogError> {
    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await
    .map_err(|source| CatalogError::Db { source })?;
    if present.is_none() {
        return Ok(0);
    }
    let version: Option<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version DESC LIMIT 1")
            .fetch_optional(&mut *connection)
            .await
            .map_err(|source| CatalogError::Db { source })?;
    Ok(version.unwrap_or(0))
}

/// Before migrations that change an existing database, copies `<name>` to
/// `<name>.pre-m<applied, 4 digits>` with `VACUUM INTO` (an existing copy is
/// kept), and removes the other `<name>.pre-*` copies (D-N-22). A new database (`applied == 0`) and a
/// current one (`applied >= expected`) get no copy. File errors arrive as
/// `sqlx::Error::Io`.
async fn pre_migration_copy(
    connection: &mut SqliteConnection,
    dir: &Path,
    name: &str,
    applied: i64,
    expected: i64,
) -> Result<(), CatalogError> {
    if applied == 0 || applied >= expected {
        return Ok(());
    }
    let io = |source: std::io::Error| CatalogError::PreMigrationCopy {
        source: sqlx::Error::Io(source),
    };
    // Keyed by the schema being left, not the package version, which stays
    // the same between releases while commit-pinned builds migrate (D-N-22).
    let target = dir.join(format!("{name}.pre-m{applied:04}"));
    let staging = dir.join(format!("{name}.pre-m{applied:04}.tmp"));
    // An existing copy is kept: a failed upgrade must not replace the good
    // pre-upgrade copy with a half-migrated database.
    let copied = !target.exists();
    if copied {
        let staging_text = staging
            .to_str()
            .ok_or_else(|| CatalogError::PreMigrationCopy {
                source: sqlx::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "data directory path is not UTF-8",
                )),
            })?;
        if staging.exists() {
            crate::fs::remove_file(&staging).map_err(io)?;
        }
        sqlx::query("VACUUM INTO ?")
            .bind(staging_text)
            .execute(&mut *connection)
            .await
            .map_err(|source| CatalogError::PreMigrationCopy { source })?;
        crate::fs::OpenOptions::new()
            .read(true)
            .open(&staging)
            .and_then(|file| file.sync_all())
            .map_err(io)?;
        crate::fs::rename(&staging, &target).map_err(io)?;
        crate::fs::sync_dir(dir).map_err(io)?;
    }
    let prefix = format!("{name}.pre-");
    for entry in crate::fs::read_dir(dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        let stale = path != target
            && path
                .file_name()
                .and_then(|file| file.to_str())
                .is_some_and(|file| file.starts_with(&prefix));
        if stale {
            crate::fs::remove_file(&path).map_err(io)?;
        }
    }
    if copied {
        tracing::info!(
            database = name,
            applied,
            expected,
            "copied the database before migrating"
        );
    } else {
        tracing::info!(
            database = name,
            applied,
            expected,
            "kept the existing pre-migration copy"
        );
    }
    Ok(())
}

/// Read every active file and both sequence rows.
pub(crate) async fn load(
    connection: &mut SqliteConnection,
) -> Result<(Catalog, TableState), CatalogError> {
    let rows = sqlx::query(
        "SELECT id, table_name, project_id, day, level, path, size_bytes, footer_size,
                row_count, min_time, max_time, min_ingest_time, max_ingest_time, fully_merged,
                deletions_applied_seq
         FROM data_files
         WHERE state = 'active'",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(CatalogError::db)?;
    let mut tables = Catalog::empty().tables;
    for row in &rows {
        insert_sorted(&mut tables, file_from_row(row)?);
    }
    let sequences = sqlx::query("SELECT table_name, flushed_seq FROM table_state")
        .fetch_all(&mut *connection)
        .await
        .map_err(CatalogError::db)?;
    let mut state = TableState {
        spans: 0,
        scores: 0,
    };
    for row in &sequences {
        let name: String = decode(row.try_get("table_name"))?;
        let stored: i64 = decode(row.try_get("flushed_seq"))?;
        let stored =
            u64::try_from(stored).map_err(|_| CatalogError::corrupt("invalid flushed_seq"))?;
        match name.as_str() {
            "spans" => state.spans = stored,
            "scores" => state.scores = stored,
            _ => return Err(CatalogError::corrupt("unknown table_name")),
        }
    }
    Ok((Catalog { tables }, state))
}

/// Read the pending deletions and the highest `deletion_seq` (spec 03 §7).
///
/// `known_seq` covers applied deletions too: the next registration takes
/// `known_seq + 1`, so a sequence number is never reused.
pub(crate) async fn load_tombstones(
    connection: &mut SqliteConnection,
) -> Result<Tombstones, CatalogError> {
    let stored: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(deletion_seq), 0) FROM deletions")
        .fetch_one(&mut *connection)
        .await
        .map_err(CatalogError::db)?;
    let known_seq =
        u64::try_from(stored).map_err(|_| CatalogError::corrupt("invalid deletion_seq"))?;
    let rows = sqlx::query(
        "SELECT id, deletion_seq, project_id, kind, cutoff, registered_at FROM deletions
         WHERE status = 'pending' ORDER BY deletion_seq",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(CatalogError::db)?;
    let mut pending = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = decode(row.try_get("id"))?;
        let parsed =
            Uuid::parse_str(&id).map_err(|_| CatalogError::corrupt("invalid deletion id"))?;
        let seq: i64 = decode(row.try_get("deletion_seq"))?;
        let seq = u64::try_from(seq).map_err(|_| CatalogError::corrupt("invalid deletion_seq"))?;
        let project: String = decode(row.try_get("project_id"))?;
        let kind: String = decode(row.try_get("kind"))?;
        let traces: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT trace_id FROM deletion_traces WHERE deletion_id = ?")
                .bind(&id)
                .fetch_all(&mut *connection)
                .await
                .map_err(CatalogError::db)?;
        let mut trace_ids = HashSet::with_capacity(traces.len());
        for blob in traces {
            trace_ids.insert(
                <[u8; 16]>::try_from(blob.as_slice())
                    .map_err(|_| CatalogError::corrupt("invalid trace_id"))?,
            );
        }
        let (users, sessions) = match kind.as_str() {
            "traces" => (HashSet::new(), HashSet::new()),
            "user" => {
                let users: Vec<String> =
                    sqlx::query_scalar("SELECT user_id FROM deletion_users WHERE deletion_id = ?")
                        .bind(&id)
                        .fetch_all(&mut *connection)
                        .await
                        .map_err(CatalogError::db)?;
                let sessions: Vec<String> = sqlx::query_scalar(
                    "SELECT session_id FROM deletion_sessions WHERE deletion_id = ?",
                )
                .bind(&id)
                .fetch_all(&mut *connection)
                .await
                .map_err(CatalogError::db)?;
                (users.into_iter().collect(), sessions.into_iter().collect())
            }
            _ => return Err(CatalogError::corrupt("unknown deletion kind")),
        };
        pending.push(PendingDeletion {
            id: parsed,
            seq,
            project: parse_project(&project)?,
            cutoff: decode(row.try_get("cutoff"))?,
            registered_at: decode(row.try_get("registered_at"))?,
            trace_ids,
            users,
            sessions,
        });
    }
    Ok(Tombstones::loaded(known_seq, pending))
}

/// Read every retention watermark (spec 03 §8) and dropped project (spec 03
/// §7). `keep_from` is `YYYY-MM-DD`, like `data_files.day`.
pub(crate) async fn load_project_states(
    connection: &mut SqliteConnection,
) -> Result<ProjectStates, CatalogError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT project_id, keep_from FROM retention_watermarks")
            .fetch_all(&mut *connection)
            .await
            .map_err(CatalogError::db)?;
    let mut keep_from = HashMap::with_capacity(rows.len());
    for (project, day) in rows {
        let project = project
            .parse()
            .map_err(|_| CatalogError::corrupt("retention watermark"))?;
        let day = NaiveDate::parse_from_str(&day, "%Y-%m-%d")
            .map_err(|_| CatalogError::corrupt("retention watermark"))?;
        keep_from.insert(project, day);
    }
    // Every row of `projects` counts as dropped, `dropping` and `dropped`
    // alike: its files are already gone and its appends refused either way.
    let dropped_rows: Vec<String> = sqlx::query_scalar("SELECT project_id FROM projects")
        .fetch_all(&mut *connection)
        .await
        .map_err(CatalogError::db)?;
    let mut dropped = HashSet::with_capacity(dropped_rows.len());
    for project in dropped_rows {
        dropped.insert(
            project
                .parse()
                .map_err(|_| CatalogError::corrupt("dropped project"))?,
        );
    }
    Ok(ProjectStates::loaded(keep_from, dropped))
}

/// Enforce spec 03 §3 over `file_sources` (D88), reading `catalog.db` directly.
///
/// Every active `spans` file is the source of exactly one active `traces` file,
/// and every active `traces` file has at least one source, each an active
/// `spans` file of the same `(project, day)`. `Storage::open` calls it on every
/// open; tests also build catalogs with direct SQL.
pub(crate) async fn check_file_sources(
    connection: &mut SqliteConnection,
) -> Result<(), CatalogError> {
    struct Active {
        id: FileId,
        table: FileTable,
        project: String,
        day: String,
    }

    let rows = sqlx::query(
        "SELECT id, table_name, project_id, day FROM data_files WHERE state = 'active'",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(CatalogError::db)?;
    // BTreeMap: when several invariants fail, report the same file id every time.
    let mut active: BTreeMap<String, Active> = BTreeMap::new();
    for row in &rows {
        let id: String = decode(row.try_get("id"))?;
        let table: String = decode(row.try_get("table_name"))?;
        let file_id = parse_file_id(&id)?;
        let project: String = decode(row.try_get("project_id"))?;
        let day: String = decode(row.try_get("day"))?;
        active.insert(
            id,
            Active {
                id: file_id,
                table: parse_file_table(&table)?,
                project,
                day,
            },
        );
    }

    let source_rows = sqlx::query("SELECT traces_file_id, spans_file_id FROM file_sources")
        .fetch_all(&mut *connection)
        .await
        .map_err(CatalogError::db)?;
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(source_rows.len());
    for row in &source_rows {
        pairs.push((
            decode(row.try_get("traces_file_id"))?,
            decode(row.try_get("spans_file_id"))?,
        ));
    }
    let mut by_spans: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut by_traces: HashMap<&str, Vec<&str>> = HashMap::new();
    for (traces, spans) in &pairs {
        by_spans
            .entry(spans.as_str())
            .or_default()
            .push(traces.as_str());
        by_traces
            .entry(traces.as_str())
            .or_default()
            .push(spans.as_str());
    }

    for (id, info) in &active {
        if info.table != FileTable::Spans {
            continue;
        }
        let derived = by_spans.get(id.as_str()).map_or(0, |traces| {
            traces
                .iter()
                .filter(|traces_file| {
                    active
                        .get(**traces_file)
                        .is_some_and(|file| file.table == FileTable::Traces)
                })
                .count()
        });
        if derived != 1 {
            return Err(CatalogError::SourcesInvariant { file: info.id.0 });
        }
    }
    for (id, info) in &active {
        if info.table != FileTable::Traces {
            continue;
        }
        let sources = by_traces
            .get(id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if sources.is_empty() {
            return Err(CatalogError::SourcesInvariant { file: info.id.0 });
        }
        for source in sources {
            let Some(span_file) = active.get(*source) else {
                return Err(CatalogError::SourcesInvariant { file: info.id.0 });
            };
            if span_file.table != FileTable::Spans
                || span_file.project != info.project
                || span_file.day != info.day
            {
                return Err(CatalogError::SourcesInvariant { file: info.id.0 });
            }
        }
    }
    Ok(())
}

fn file_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<FileMeta, CatalogError> {
    let level: i64 = decode(row.try_get("level"))?;
    let fully_merged: i64 = decode(row.try_get("fully_merged"))?;
    let day: String = decode(row.try_get("day"))?;
    let id: String = decode(row.try_get("id"))?;
    let project: String = decode(row.try_get("project_id"))?;
    let table: String = decode(row.try_get("table_name"))?;
    Ok(FileMeta {
        id: parse_file_id(&id)?,
        table: parse_file_table(&table)?,
        project: parse_project(&project)?,
        day: parse_day(&day)?,
        level: parse_level(level)?,
        path: decode(row.try_get("path"))?,
        size: parse_u64(decode(row.try_get("size_bytes"))?, "invalid size")?,
        footer_size: parse_u64(decode(row.try_get("footer_size"))?, "invalid footer_size")?,
        rows: parse_u64(decode(row.try_get("row_count"))?, "invalid row_count")?,
        min_time: decode(row.try_get("min_time"))?,
        max_time: decode(row.try_get("max_time"))?,
        min_ingest_time: decode(row.try_get("min_ingest_time"))?,
        max_ingest_time: decode(row.try_get("max_ingest_time"))?,
        fully_merged: parse_flag(fully_merged)?,
        deletions_applied_seq: parse_u64(
            decode(row.try_get("deletions_applied_seq"))?,
            "deletions_applied_seq",
        )?,
    })
}

fn decode<T>(result: Result<T, sqlx::Error>) -> Result<T, CatalogError> {
    result.map_err(|source| match source {
        sqlx::Error::ColumnDecode { .. } => CatalogError::corrupt("invalid column"),
        other => CatalogError::Db { source: other },
    })
}

pub(super) fn parse_file_id(text: &str) -> Result<FileId, CatalogError> {
    if text.len() != 36 {
        return Err(CatalogError::corrupt("invalid file id"));
    }
    Uuid::parse_str(text)
        .map(FileId)
        .map_err(|_| CatalogError::corrupt("invalid file id"))
}

fn parse_project(text: &str) -> Result<ProjectId, CatalogError> {
    text.parse()
        .map_err(|_| CatalogError::corrupt("invalid project_id"))
}

fn parse_day(text: &str) -> Result<NaiveDate, CatalogError> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| CatalogError::corrupt("invalid day"))
}

fn parse_file_table(text: &str) -> Result<FileTable, CatalogError> {
    match text {
        "spans" => Ok(FileTable::Spans),
        "traces" => Ok(FileTable::Traces),
        "scores" => Ok(FileTable::Scores),
        _ => Err(CatalogError::corrupt("unknown table_name")),
    }
}

fn parse_level(value: i64) -> Result<u8, CatalogError> {
    match value {
        0 | 1 => u8::try_from(value).map_err(|_| CatalogError::corrupt("invalid level")),
        _ => Err(CatalogError::corrupt("invalid level")),
    }
}

fn parse_u64(value: i64, what: &'static str) -> Result<u64, CatalogError> {
    u64::try_from(value).map_err(|_| CatalogError::corrupt(what))
}

fn parse_flag(value: i64) -> Result<bool, CatalogError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CatalogError::corrupt("invalid fully_merged")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::tests_support::dir_digest;
    use sqlx::Connection;

    const PROJECT: &str = "018f6b4e-7c1a-7c1a-8c1a-000000000001";
    const DAY: &str = "2024-06-01";

    fn file_id(n: u8) -> Uuid {
        Uuid::from_u128(0x018f6b4e_7c1a_7c1a_8c1a_000000000000_u128 + u128::from(n))
    }

    async fn insert_file(connection: &mut SqliteConnection, id: Uuid, table: &str, day: &str) {
        sqlx::query(
            "INSERT INTO data_files (
                id, table_name, project_id, day, level, path, size_bytes, footer_size,
                row_count, min_time, max_time, min_ingest_time, max_ingest_time,
                fully_merged, created_at
             ) VALUES (?, ?, ?, ?, 0, ?, 1, 1, 1, 0, 0, 0, 0, 0, 1)",
        )
        .bind(id.to_string())
        .bind(table)
        .bind(PROJECT)
        .bind(day)
        .bind(format!("objects/{id}"))
        .execute(&mut *connection)
        .await
        .unwrap();
    }

    async fn insert_source(connection: &mut SqliteConnection, traces: Uuid, spans: Uuid) {
        sqlx::query("INSERT INTO file_sources (traces_file_id, spans_file_id) VALUES (?, ?)")
            .bind(traces.to_string())
            .bind(spans.to_string())
            .execute(&mut *connection)
            .await
            .unwrap();
    }

    async fn has_file_sources(connection: &mut SqliteConnection) -> bool {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('file_sources') ORDER BY cid")
                .fetch_all(&mut *connection)
                .await
                .unwrap();
        let index: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'file_sources_by_spans'",
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        columns == vec!["traces_file_id".to_owned(), "spans_file_id".to_owned()] && index == 1
    }

    /// Builds a catalog with direct SQL, then runs the spec 03 §3 check.
    async fn check_sources(
        files: &[(Uuid, &str, &str)],
        sources: &[(Uuid, Uuid)],
    ) -> Result<(), CatalogError> {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        for (id, table, day) in files {
            insert_file(&mut connection, *id, table, day).await;
        }
        for (traces, spans) in sources {
            insert_source(&mut connection, *traces, *spans).await;
        }
        check_file_sources(&mut connection).await
    }

    #[tokio::test]
    async fn migration_applies_to_a_fresh_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        assert!(has_file_sources(&mut connection).await);
    }

    /// Lists the `catalog.db.pre-*` files in `dir`.
    fn pre_copies(dir: &Path) -> Vec<String> {
        crate::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("catalog.db.pre-"))
            .collect()
    }

    #[tokio::test]
    async fn opening_an_older_catalog_keeps_a_pre_migration_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        // An older binary left only migration 0001 applied.
        let mut first_only = sqlx::migrate!("./migrations/catalog");
        let first = first_only.migrations[..1].to_vec();
        first_only.migrations = std::borrow::Cow::Owned(first);
        let mut older = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        first_only.run(&mut older).await.unwrap();
        drop(older);
        std::fs::write(dir.path().join("catalog.db.pre-0.0.0-old"), b"old").unwrap();

        let connection = open_catalog_db(dir.path()).await.unwrap();
        drop(connection);

        let copy = dir.path().join("catalog.db.pre-m0001");
        assert!(copy.is_file());
        assert_eq!(
            pre_copies(dir.path()),
            vec![copy.file_name().unwrap().to_str().unwrap().to_owned()]
        );
        let mut copy = SqliteConnectOptions::new()
            .filename(&copy)
            .connect()
            .await
            .unwrap();
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&mut copy)
            .await
            .unwrap();
        assert_eq!(version, 1);
    }

    #[tokio::test]
    async fn an_existing_pre_migration_copy_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let mut first_only = sqlx::migrate!("./migrations/catalog");
        let first = first_only.migrations[..1].to_vec();
        first_only.migrations = std::borrow::Cow::Owned(first);
        let mut older = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        first_only.run(&mut older).await.unwrap();
        drop(older);
        let copy = dir.path().join("catalog.db.pre-m0001");
        std::fs::write(&copy, b"known content").unwrap();

        drop(open_catalog_db(dir.path()).await.unwrap());

        assert_eq!(std::fs::read(&copy).unwrap(), b"known content");
    }

    #[tokio::test]
    async fn a_new_or_current_catalog_gets_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        drop(open_catalog_db(dir.path()).await.unwrap());
        drop(open_catalog_db(dir.path()).await.unwrap());
        assert!(pre_copies(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn migration_applies_to_an_m2_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        // An M2 catalog has only migration 0001 applied. Drop 0002's objects
        // and its migration row, then let open_catalog_db re-apply it.
        sqlx::query("DROP TABLE file_sources")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 2")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        assert!(has_file_sources(&mut connection).await);
        let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(versions, 7);
    }

    #[tokio::test]
    async fn migration_applies_to_an_m4_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        insert_file(&mut connection, file_id(1), "spans", DAY).await;
        // An M4 catalog has only migrations 0001 and 0002 applied. Undo every
        // migration from 0003 on, newest first, then let open_catalog_db
        // re-apply them.
        for statement in [
            "DROP TABLE projects",
            "DROP TABLE media",
            "DROP TABLE retention_watermarks",
            "DROP INDEX deletions_pending",
            "DROP TABLE deletion_sessions",
            "DROP TABLE deletion_users",
            "DROP TABLE deletion_traces",
            "DROP TABLE deletions",
            "ALTER TABLE data_files DROP COLUMN deletions_applied_seq",
            "DELETE FROM _sqlx_migrations WHERE version >= 3",
        ] {
            sqlx::query(statement)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        drop(connection);

        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        let column: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('data_files')
             WHERE name = 'deletions_applied_seq'",
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(column, 1);
        let seq: i64 = sqlx::query_scalar("SELECT deletions_applied_seq FROM data_files")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(seq, 0);
        for table in [
            "deletions",
            "deletion_traces",
            "deletion_users",
            "deletion_sessions",
            "retention_watermarks",
            "media",
            "projects",
        ] {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(count, 1, "{table} is missing");
        }
    }

    #[tokio::test]
    async fn deleting_a_data_file_removes_its_sources() {
        for deleted in [1_u8, 2] {
            let dir = tempfile::tempdir().unwrap();
            let mut connection = open_catalog_db(dir.path()).await.unwrap();
            let spans = file_id(1);
            let traces = file_id(2);
            insert_file(&mut connection, spans, "spans", DAY).await;
            insert_file(&mut connection, traces, "traces", DAY).await;
            insert_source(&mut connection, traces, spans).await;
            sqlx::query("DELETE FROM data_files WHERE id = ?")
                .bind(file_id(deleted).to_string())
                .execute(&mut connection)
                .await
                .unwrap();
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_sources")
                .fetch_one(&mut connection)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[tokio::test]
    async fn file_table_round_trips_through_table_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        insert_file(&mut connection, file_id(1), "spans", DAY).await;
        insert_file(&mut connection, file_id(2), "traces", DAY).await;
        insert_file(&mut connection, file_id(3), "scores", DAY).await;
        let (catalog, _) = load(&mut connection).await.unwrap();
        assert_eq!(catalog.test_files(FileTable::Spans).len(), 1);
        assert_eq!(catalog.test_files(FileTable::Traces).len(), 1);
        assert_eq!(catalog.test_files(FileTable::Scores).len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_table_name_fails_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        insert_file(&mut connection, file_id(1), "unknown", DAY).await;
        let error = load(&mut connection).await.unwrap_err();
        assert!(matches!(error, CatalogError::Corrupt { what } if what == "unknown table_name"));
    }

    #[tokio::test]
    async fn sources_check_accepts_one_traces_file_per_spans_file() {
        let spans = file_id(1);
        let traces = file_id(2);
        assert!(
            check_sources(
                &[(spans, "spans", DAY), (traces, "traces", DAY)],
                &[(traces, spans)],
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn sources_check_rejects_a_spans_file_without_a_traces_file() {
        let spans = file_id(1);
        let error = check_sources(&[(spans, "spans", DAY)], &[])
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogError::SourcesInvariant { file } if file == spans));
    }

    #[tokio::test]
    async fn sources_check_rejects_a_spans_file_with_two_traces_files() {
        let spans = file_id(1);
        let first = file_id(2);
        let second = file_id(3);
        let error = check_sources(
            &[
                (spans, "spans", DAY),
                (first, "traces", DAY),
                (second, "traces", DAY),
            ],
            &[(first, spans), (second, spans)],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CatalogError::SourcesInvariant { file } if file == spans));
    }

    #[tokio::test]
    async fn sources_check_rejects_a_traces_file_without_a_source() {
        let traces = file_id(1);
        let error = check_sources(&[(traces, "traces", DAY)], &[])
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogError::SourcesInvariant { file } if file == traces));
    }

    #[tokio::test]
    async fn sources_check_rejects_a_source_in_another_day() {
        let spans = file_id(1);
        let traces = file_id(2);
        let error = check_sources(
            &[(spans, "spans", DAY), (traces, "traces", "2024-06-02")],
            &[(traces, spans)],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CatalogError::SourcesInvariant { file } if file == traces));
    }

    /// Sets `PRAGMA user_version` of `<dir>/catalog.db` through its own
    /// connection, which is closed before the function returns.
    async fn set_user_version(dir: &Path, version: u16) {
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .connect()
            .await
            .unwrap();
        sqlx::query(AssertSqlSafe(format!("PRAGMA user_version = {version}")))
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
    }

    /// Reads `PRAGMA user_version` through a second connection.
    async fn user_version(dir: &Path) -> i64 {
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let version = read_user_version(&mut connection).await.unwrap();
        connection.close().await.unwrap();
        version
    }

    /// Creates `<dir>/catalog.db` with the migrations up to and including
    /// `last` applied, as an older binary left it, and closes it.
    async fn migrated_through(dir: &Path, last: i64) {
        let mut older_migrations = sqlx::migrate!("./migrations/catalog");
        let kept: Vec<_> = older_migrations
            .migrations
            .iter()
            .filter(|migration| migration.version <= last)
            .cloned()
            .collect();
        older_migrations.migrations = std::borrow::Cow::Owned(kept);
        let mut older = SqliteConnectOptions::new()
            .filename(dir.join("catalog.db"))
            .create_if_missing(true)
            .connect()
            .await
            .unwrap();
        older_migrations.run(&mut older).await.unwrap();
        older.close().await.unwrap();
    }

    /// Opens `dir`, which must be refused, and returns the error.
    async fn refused(dir: &Path) -> CatalogError {
        match open_catalog_db(dir).await {
            Ok(_) => panic!("the open of the data directory should be refused"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn a_new_catalog_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let connection = open_catalog_db(dir.path()).await.unwrap();
        connection.close().await.unwrap();
        assert_eq!(user_version(dir.path()).await, i64::from(FORMAT_VERSION));

        let connection = open_catalog_db(dir.path()).await.unwrap();
        connection.close().await.unwrap();
        assert_eq!(user_version(dir.path()).await, i64::from(FORMAT_VERSION));
    }

    #[tokio::test]
    async fn a_newer_marker_is_refused_before_anything_changes() {
        let dir = tempfile::tempdir().unwrap();
        let connection = open_catalog_db(dir.path()).await.unwrap();
        connection.close().await.unwrap();
        set_user_version(dir.path(), 2).await;
        let before = dir_digest(dir.path());

        let error = refused(dir.path()).await;
        assert!(matches!(
            error,
            CatalogError::FormatTooNew {
                found: 2,
                supported: 1
            }
        ));
        assert_eq!(dir_digest(dir.path()), before);
    }

    #[tokio::test]
    async fn a_newer_marker_is_refused_before_pending_migrations() {
        let dir = tempfile::tempdir().unwrap();
        migrated_through(dir.path(), 1).await;
        set_user_version(dir.path(), 2).await;
        let before = dir_digest(dir.path());

        let error = refused(dir.path()).await;
        assert!(matches!(
            error,
            CatalogError::FormatTooNew {
                found: 2,
                supported: 1
            }
        ));
        assert_eq!(dir_digest(dir.path()), before);
        assert!(pre_copies(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn a_pre_release_catalog_with_data_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        insert_file(&mut connection, file_id(1), "spans", DAY).await;
        connection.close().await.unwrap();
        set_user_version(dir.path(), 0).await;

        assert!(matches!(refused(dir.path()).await, CatalogError::PreFormat));
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.path().join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM data_files")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn a_pre_release_catalog_with_data_and_pending_migrations_is_refused_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        migrated_through(dir.path(), 1).await;
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.path().join("catalog.db"))
            .connect()
            .await
            .unwrap();
        insert_file(&mut connection, file_id(1), "spans", DAY).await;
        connection.close().await.unwrap();
        let before = dir_digest(dir.path());

        assert!(matches!(refused(dir.path()).await, CatalogError::PreFormat));
        assert!(pre_copies(dir.path()).is_empty());
        assert_eq!(dir_digest(dir.path()), before);
        let mut connection = SqliteConnectOptions::new()
            .filename(dir.path().join("catalog.db"))
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(applied, 1);
    }

    #[tokio::test]
    async fn an_empty_catalog_without_a_marker_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let connection = open_catalog_db(dir.path()).await.unwrap();
        connection.close().await.unwrap();
        set_user_version(dir.path(), 0).await;

        let connection = open_catalog_db(dir.path()).await.unwrap();
        connection.close().await.unwrap();
        assert_eq!(user_version(dir.path()).await, i64::from(FORMAT_VERSION));
    }

    #[tokio::test]
    async fn an_empty_catalog_with_pending_migrations_migrates_then_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        migrated_through(dir.path(), 1).await;
        assert_eq!(user_version(dir.path()).await, 0);

        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        assert!(has_file_sources(&mut connection).await);
        connection.close().await.unwrap();
        assert_eq!(user_version(dir.path()).await, i64::from(FORMAT_VERSION));
    }

    #[tokio::test]
    async fn an_unknown_applied_migration_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (9999, 'x', 1, X'00', 0)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        connection.close().await.unwrap();

        assert!(matches!(
            refused(dir.path()).await,
            CatalogError::SchemaTooNew { version: 9999 }
        ));
    }

    #[tokio::test]
    async fn a_vacuum_copy_keeps_the_marker() {
        // The pre-migration copy and the snapshot are both `VACUUM INTO`.
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        let copy = dir.path().join("copy.db");
        sqlx::query("VACUUM INTO ?")
            .bind(copy.to_str().unwrap())
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();

        let mut copied = SqliteConnectOptions::new()
            .filename(&copy)
            .read_only(true)
            .connect()
            .await
            .unwrap();
        let version = read_user_version(&mut copied).await.unwrap();
        assert_eq!(version, i64::from(FORMAT_VERSION));
    }

    /// Migrations that change the format (spec 03 § 12). `migrations_only_add`
    /// skips these. Add a migration's version here together with the
    /// `FORMAT_VERSION` bump it needs.
    const FORMAT_CHANGING_MIGRATIONS: &[i64] = &[];

    /// A migration that alters an existing table, with a row it must keep.
    struct AlteringCase {
        /// The migration under test.
        version: i64,
        /// Inserts the row as the migration before `version` left the table.
        seed: &'static str,
        /// Reads the row back after the open, one text value per row.
        read: &'static str,
        /// The value `read` returns: the row is intact, and the new column has
        /// its default.
        expected: &'static str,
    }

    const ALTERING_CASES: &[AlteringCase] = &[
        AlteringCase {
            version: 3,
            seed: "INSERT INTO data_files (id, table_name, project_id, day, level, path,
                size_bytes, footer_size, row_count, min_time, max_time, min_ingest_time,
                max_ingest_time, fully_merged, created_at)
                VALUES ('018f6b4e-7c1a-7c1a-8c1a-0000000000f1', 'spans',
                '018f6b4e-7c1a-7c1a-8c1a-000000000001', '2024-06-01', 0, 'objects/kept',
                42, 1, 1, 0, 0, 0, 0, 0, 1)",
            read: "SELECT id || '|' || path || '|' || size_bytes || '|' || deletions_applied_seq
                FROM data_files",
            expected: "018f6b4e-7c1a-7c1a-8c1a-0000000000f1|objects/kept|42|0",
        },
        AlteringCase {
            version: 7,
            seed: "INSERT INTO media (project_id, sha256, mime_type, size_bytes,
                first_seen_at, last_seen_at)
                VALUES ('018f6b4e-7c1a-7c1a-8c1a-000000000001',
                '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef',
                'image/png', 42, 1, 2)",
            read: "SELECT sha256 || '|' || size_bytes || '|' || newest_span_start FROM media",
            expected: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef|42|0",
        },
    ];

    /// The statements of a migration: `--` comments dropped, split on `;`,
    /// whitespace collapsed, empty ones dropped. No migration has a `--` or a
    /// `;` inside a string literal.
    fn statements(sql: &str) -> Vec<String> {
        let code: Vec<&str> = sql
            .lines()
            .map(|line| line.split("--").next().unwrap_or_default())
            .collect();
        code.join("\n")
            .split(';')
            .map(|statement| statement.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|statement| !statement.is_empty())
            .collect()
    }

    /// Whether a statement only adds to the schema: it creates a table or an
    /// index, or adds a column that has a default or is nullable.
    fn is_additive(statement: &str) -> bool {
        let upper = statement.to_ascii_uppercase();
        if upper.starts_with("CREATE TABLE ")
            || upper.starts_with("CREATE INDEX ")
            || upper.starts_with("CREATE UNIQUE INDEX ")
        {
            return true;
        }
        let words: Vec<&str> = upper.split_whitespace().collect();
        match words.as_slice() {
            // ALTER TABLE <name> ADD COLUMN <column> <type> ...
            ["ALTER", "TABLE", _, "ADD", "COLUMN", _, ..] => {
                let definition = &words[6..];
                let has_default = definition.contains(&"DEFAULT");
                let not_null = definition
                    .windows(2)
                    .any(|pair| pair[0] == "NOT" && pair[1] == "NULL");
                has_default || !not_null
            }
            _ => false,
        }
    }

    #[test]
    fn migrations_only_add() {
        for migration in sqlx::migrate!("./migrations/catalog").migrations.iter() {
            if FORMAT_CHANGING_MIGRATIONS.contains(&migration.version) {
                continue;
            }
            for statement in statements(migration.sql.as_str()) {
                assert!(
                    is_additive(&statement),
                    "migration {:04} is not additive: a format change. Bump FORMAT_VERSION (spec 03 § 12), then list the migration in FORMAT_CHANGING_MIGRATIONS.",
                    migration.version,
                );
            }
        }
    }

    #[test]
    fn every_altering_migration_has_a_case() {
        for migration in sqlx::migrate!("./migrations/catalog").migrations.iter() {
            let alters = statements(migration.sql.as_str())
                .iter()
                .any(|statement| statement.to_ascii_uppercase().starts_with("ALTER TABLE "));
            if alters {
                assert!(
                    ALTERING_CASES
                        .iter()
                        .any(|case| case.version == migration.version),
                    "migration {:04} alters a table: add a case to ALTERING_CASES",
                    migration.version,
                );
            }
        }
    }

    #[tokio::test]
    async fn migrations_that_alter_a_table_keep_its_rows() {
        for case in ALTERING_CASES {
            let dir = tempfile::tempdir().unwrap();
            // The release before the migration: the migrations below it are
            // applied and the marker is set, as a released build leaves it.
            migrated_through(dir.path(), case.version - 1).await;
            set_user_version(dir.path(), FORMAT_VERSION).await;
            let mut older = SqliteConnectOptions::new()
                .filename(dir.path().join("catalog.db"))
                .connect()
                .await
                .unwrap();
            sqlx::query(case.seed).execute(&mut older).await.unwrap();
            older.close().await.unwrap();

            let mut connection = open_catalog_db(dir.path()).await.unwrap();
            let rows: Vec<String> = sqlx::query_scalar(case.read)
                .fetch_all(&mut connection)
                .await
                .unwrap();
            assert_eq!(
                rows,
                vec![case.expected.to_owned()],
                "migration {:04} must keep the row",
                case.version,
            );
        }
    }

    /// The committed dump of a fresh catalog's objects, one line each.
    const SCHEMA_DUMP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/catalog/schema.txt");

    #[tokio::test]
    async fn the_schema_matches_the_committed_dump() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open_catalog_db(dir.path()).await.unwrap();
        let objects: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' ORDER BY name",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap();
        let actual: String = objects
            .iter()
            .map(|(kind, name, table, sql)| {
                let sql = sql
                    .as_deref()
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{kind} | {name} | {table} | {sql}\n")
            })
            .collect();

        if std::env::var("CETRA_BLESS_SCHEMA").as_deref() == Ok("1") {
            std::fs::write(SCHEMA_DUMP, &actual).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(SCHEMA_DUMP).expect("read src/catalog/schema.txt");
        assert!(
            actual == committed,
            "the schema of a fresh catalog differs from src/catalog/schema.txt. A migration changed the schema: update the SQL in docs/spec/03-storage.md § 4 by hand, then rewrite the file with CETRA_BLESS_SCHEMA=1 cargo test -p cetra-storage the_schema_matches_the_committed_dump",
        );
    }
}
