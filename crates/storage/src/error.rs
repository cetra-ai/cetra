//! Errors returned by storage operations.
//!
//! Messages name the table, segment, offset, sequence number, or size.
//! They never include row content or attribute values.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use cetra_column::ColumnError;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EngineErrorKind {
    ResourcesExhausted,
    Parquet,
    ObjectStore,
    #[cfg(test)]
    Io,
    Column,
    Busy,
    Internal,
}

impl EngineErrorKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ResourcesExhausted => "resources_exhausted",
            Self::Parquet => "parquet",
            Self::ObjectStore => "object_store",
            #[cfg(test)]
            Self::Io => "io",
            Self::Column => "column",
            Self::Busy => "busy",
            Self::Internal => "internal",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::ResourcesExhausted => "query memory or spill limit exceeded",
            Self::Parquet => "corrupt parquet file",
            Self::ObjectStore => "object store read error",
            #[cfg(test)]
            Self::Io => "query I/O error",
            Self::Column => "columnar batch error",
            Self::Busy => "too many queries are running",
            Self::Internal => "internal query engine error",
        }
    }
}

/// Opaque, privacy-safe internal error from the query or compaction read engine.
///
/// [`fmt::Display`] and [`fmt::Debug`] only ever emit static category labels and
/// never expose row values, prompts, query text, or object paths (`AGENTS.md` § Privacy).
#[derive(Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.kind.message())]
pub struct EngineError {
    kind: EngineErrorKind,
}

impl fmt::Debug for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EngineError")
            .field(&self.kind.label())
            .finish()
    }
}

impl EngineError {
    pub(crate) fn kind(self) -> EngineErrorKind {
        self.kind
    }

    pub(crate) fn kind_label(self) -> &'static str {
        self.kind.label()
    }

    #[cfg(test)]
    pub(crate) fn is_busy(self) -> bool {
        self.kind == EngineErrorKind::Busy
    }

    pub(crate) fn to_query_error(self) -> cetra_query::QueryError {
        match self.kind {
            EngineErrorKind::ResourcesExhausted => cetra_query::QueryError::TooLarge,
            EngineErrorKind::Busy => cetra_query::QueryError::Busy,
            EngineErrorKind::Parquet => {
                cetra_query::QueryError::CorruptData("corrupt parquet file".to_owned())
            }
            EngineErrorKind::ObjectStore => {
                cetra_query::QueryError::StorageIo("storage read error".to_owned())
            }
            _ => cetra_query::QueryError::internal(self.kind.label()),
        }
    }

    pub(crate) fn from_query(err: &cetra_query::QueryError) -> Self {
        let kind = match err {
            cetra_query::QueryError::TooLarge => EngineErrorKind::ResourcesExhausted,
            cetra_query::QueryError::CorruptData(_) => EngineErrorKind::Parquet,
            cetra_query::QueryError::StorageIo(_) => EngineErrorKind::ObjectStore,
            cetra_query::QueryError::Column(_) => EngineErrorKind::Column,
            cetra_query::QueryError::Busy => EngineErrorKind::Busy,
            _ => EngineErrorKind::Internal,
        };
        Self { kind }
    }

    pub(crate) fn from_parquet(err: &parquet::errors::ParquetError) -> Self {
        let kind = if crate::verify::is_store_error(err) {
            EngineErrorKind::ObjectStore
        } else {
            EngineErrorKind::Parquet
        };
        Self { kind }
    }

    pub(crate) fn from_column(_err: &ColumnError) -> Self {
        Self {
            kind: EngineErrorKind::Column,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_resources_exhausted() -> Self {
        Self {
            kind: EngineErrorKind::ResourcesExhausted,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_io() -> Self {
        Self {
            kind: EngineErrorKind::Io,
        }
    }

    pub(crate) fn internal(_reason: &'static str) -> Self {
        Self {
            kind: EngineErrorKind::Internal,
        }
    }
}

/// Constructs an opaque [`EngineError`] for downstream crate tests without exposing
/// internal engine types.
#[doc(hidden)]
pub fn engine_error_for_test(_msg: &str) -> EngineError {
    EngineError::internal("test error")
}

/// A `[storage]` value failed a parse or a bound check.
///
/// The message names the field and the rule. It never includes the rejected input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `field` is a config path such as `storage.wal.segment_bytes`.
    /// `rule` is a static description of the constraint.
    #[error("{field}: {rule}")]
    Invalid {
        /// Dotted config path, for example `storage.wal.segment_bytes`.
        field: &'static str,
        /// Static rule. Never the rejected input.
        rule: &'static str,
    },
}

impl ConfigError {
    pub(crate) fn invalid(field: &'static str, rule: &'static str) -> Self {
        Self::Invalid { field, rule }
    }
}

/// `Storage::open` failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Another process holds `<data_dir>/LOCK`.
    #[error("data directory is locked: {path}")]
    Locked {
        /// The data directory that is already open.
        path: PathBuf,
    },
    /// Creating the data directory or taking the lock failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being done, for example `creating WAL directory`.
        action: &'static str,
        /// Path involved in the failed call.
        path: PathBuf,
        /// The filesystem error.
        #[source]
        source: io::Error,
    },
    /// A WAL segment is corrupt somewhere other than a torn tail.
    #[error("corrupt WAL table {table}, segment {segment}, offset {offset}")]
    WalCorrupt {
        /// Table name, `spans` or `scores`.
        table: &'static str,
        /// Segment file name.
        segment: String,
        /// Byte offset of the bad record.
        offset: u64,
    },
    /// Replaying a WAL failed for a reason other than a corrupt record.
    #[error(transparent)]
    WalReplay(#[from] WalError),
    /// `catalog.db` could not be opened or migrated.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The trace anchor state could not be rebuilt.
    #[error(transparent)]
    Anchor(#[from] AnchorError),
    /// The object store could not be opened, or a startup cleanup failed.
    #[error(transparent)]
    Objects(#[from] ObjectError),
}

/// `append_spans` or `append_scores` failed.
///
/// The server maps these to HTTP: `Backpressure` is 429, `WalFailed` and
/// `ShuttingDown` and `DiskFull` are 503, and `InvalidBatch` is a bug in the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AppendError {
    /// The batch is empty, has the wrong schema, or names another project.
    #[error("invalid batch: {reason}")]
    InvalidBatch {
        /// Static reason. Never row content.
        reason: &'static str,
    },
    /// The memtable is at `storage.memtable.hard_limit`.
    #[error("memtable is at its hard limit")]
    Backpressure,
    /// The WAL writer has failed and will not accept more writes.
    #[error("WAL writer failed")]
    WalFailed,
    /// Shutdown has started.
    #[error("storage is shutting down")]
    ShuttingDown,
    /// The project was dropped (spec 03 §7). The server maps it to 404.
    #[error("project is dropped")]
    ProjectDropped,
    /// Free disk is under `storage.min_free_bytes` (spec 03 §10). The server maps it to 503.
    #[error("free disk is under storage.min_free_bytes")]
    DiskFull,
}

/// Shutdown failed, or a later maintenance call failed.
///
/// Returned by `shutdown`, `apply_retention` and `drop_project`.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The table's writer stopped after a WAL error.
    #[error("WAL writer for {table} failed")]
    WalFailed {
        /// `spans` or `scores`.
        table: &'static str,
    },
    /// The table's writer task panicked.
    #[error("WAL writer for {table} panicked")]
    WriterPanicked {
        /// `spans` or `scores`.
        table: &'static str,
    },
    /// The catalog commit failed.
    #[error(transparent)]
    Commit(#[from] CommitError),
    /// `catalog.db` failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// Deleting a dropped project's media failed (spec 03 §7). The project is
    /// already dropped; GC finishes the media.
    #[error(transparent)]
    Media(#[from] MediaError),
}

/// Why [`crate::Storage::flush_now`] failed.
#[derive(Debug, thiserror::Error)]
pub enum FlushNowError {
    /// A WAL writer has stopped on an I/O error; nothing more will flush.
    #[error("the {table} WAL writer has failed")]
    WriterFailed {
        /// `spans` or `scores`.
        table: &'static str,
    },
    /// The flush did not commit within the timeout.
    #[error("the {table} flush did not commit within {timeout:?}")]
    Timeout {
        /// `spans` or `scores`.
        table: &'static str,
        /// The timeout the caller passed.
        timeout: std::time::Duration,
    },
    /// The storage is shutting down.
    #[error("storage is shutting down")]
    ShuttingDown,
}

/// `Storage::query` failed.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The native query engine failed to initialize or project a snapshot.
    #[error(transparent)]
    Internal(#[from] EngineError),
    /// No query memory came free within `query.admission_wait`. The server
    /// maps this to 503 `busy` (spec 09).
    #[error("too many queries are running")]
    Busy,
}

/// Inserting a batch into a memtable failed.
///
/// The message names the column. It never includes row content.
#[derive(Debug, thiserror::Error)]
pub enum MemtableError {
    /// A timestamp cannot be turned into a UTC day.
    #[error("time column is outside the supported range")]
    TimeOutOfRange,
    /// The table's time column is absent.
    #[error("batch is missing column {column}")]
    MissingColumn {
        /// Column name, for example `start_time`.
        column: &'static str,
    },
    /// The time column is not nanosecond timestamps, or it contains a null.
    #[error("column {column} has an unexpected type or a null")]
    BadColumn {
        /// Column name.
        column: &'static str,
    },
    /// Filtering or concatenating batches failed.
    #[error("building a memtable batch")]
    Column(#[from] ColumnError),
}

impl MemtableError {
    pub(crate) fn from_column(err: ColumnError) -> Self {
        Self::Column(err)
    }
}

/// A WAL codec, segment, or replay failure.
///
/// Callers outside the WAL map this into [`OpenError`] or [`AppendError`].
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    /// The binary frame payload is not one batch of the table's schema.
    #[error("invalid WAL payload: {reason}")]
    InvalidPayload {
        /// Static reason. Never row content.
        reason: &'static str,
    },
    /// A record failed its checksum, ran backwards, or was not a batch of this table.
    ///
    /// `segment` is the file name. `offset` is the byte where the bad record starts.
    #[error("corrupt WAL table {table}, segment {segment}, offset {offset}")]
    Corrupt {
        /// `spans` or `scores`.
        table: &'static str,
        /// Segment file name, for example `00000000000000000042.wal`.
        segment: String,
        /// Byte offset of the corrupt record.
        offset: u64,
    },
    /// A segment's header is not one this build reads (spec 03 § 12).
    #[error("WAL segment {segment} of table {table}: {problem}")]
    Header {
        /// `spans` or `scores`.
        table: &'static str,
        /// Segment file name, for example `00000000000000000042.wal`.
        segment: String,
        /// What is wrong with the header.
        problem: HeaderProblem,
    },
    /// A segment file or its directory could not be read or written.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being done, for example `syncing WAL segment`.
        action: &'static str,
        /// File or directory involved in the failed call.
        path: PathBuf,
        /// The filesystem error.
        #[source]
        source: io::Error,
    },
}

/// Why a WAL segment's header is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderProblem {
    /// The magic differs, or the file is shorter than the header and not a
    /// prefix of it. Files written before format versions have no header.
    #[error("not a WAL segment of this storage (it may predate format versions)")]
    NotAWalSegment,
    /// The version is not the one this build reads. No older version exists,
    /// so the segment was written by a newer release.
    #[error(
        "format {found} is not the {supported} this build reads; the segment was written by a different release: run the release that wrote it, or restore the backup taken before the upgrade"
    )]
    NewerFormat {
        /// The version in the segment header.
        found: u16,
        /// The version this build reads.
        supported: u16,
    },
    /// The flags are not 0.
    #[error("unknown flags; run the newer release")]
    UnknownFlags,
}

impl WalError {
    /// An I/O failure that names the action and the path.
    pub(crate) fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn from_column(err: &ColumnError) -> Self {
        match *err {
            ColumnError::InvalidFrame { reason }
            | ColumnError::InvalidBatch { reason }
            | ColumnError::Overflow { reason } => Self::InvalidPayload { reason },
            ColumnError::InvalidUtf8 => Self::InvalidPayload {
                reason: "invalid utf-8 in column data",
            },
            ColumnError::MemoryLimit { .. } => Self::InvalidPayload {
                reason: "memory limit exceeded",
            },
        }
    }
}

/// Opening or reading `catalog.db` failed.
///
/// `Corrupt` uses a static description; `SourcesInvariant` names the offending
/// file id. Neither carries row content, attribute values or secrets.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The database file could not be opened.
    #[error("catalog {path}: {source}")]
    Open {
        /// `<data_dir>/catalog.db`.
        path: PathBuf,
        /// The SQLite error.
        #[source]
        source: sqlx::Error,
    },
    /// A migration could not be applied.
    #[error("catalog migration: {source}")]
    Migrate {
        /// The migration error.
        #[source]
        source: sqlx::migrate::MigrateError,
    },
    /// The copy of `catalog.db` taken before a migration failed (D-N-22).
    #[error("catalog pre-migration copy: {source}")]
    PreMigrationCopy {
        /// The SQLite error, or the file error as `sqlx::Error::Io`.
        #[source]
        source: sqlx::Error,
    },
    /// A query failed.
    #[error("catalog database: {source}")]
    Db {
        /// The SQLite error.
        #[source]
        source: sqlx::Error,
    },
    /// A stored value is not one the catalog can represent.
    #[error("corrupt catalog: {what}")]
    Corrupt {
        /// Static description, for example `unknown table_name`.
        what: &'static str,
    },
    /// Spec 03 §3's `file_sources` invariant failed for one file.
    #[error("catalog file_sources invariant: file {file}")]
    SourcesInvariant {
        /// The offending `data_files.id`.
        file: Uuid,
    },
    /// The directory was written by a newer format (spec 03 § 12).
    #[error(
        "this data directory has format version {found}, which this build (format {supported}) does not read; run the newer release, or restore the backup taken before the upgrade"
    )]
    FormatTooNew {
        /// The `PRAGMA user_version` of `catalog.db`.
        found: i64,
        /// The format version this build reads and writes.
        supported: u16,
    },
    /// A directory from before format versions existed (pre-release).
    #[error(
        "this data directory has no format version: it was written before format versions were introduced; create a new data directory"
    )]
    PreFormat,
    /// `catalog.db` has applied a migration this build does not have.
    #[error(
        "catalog.db has applied migration {version}, which this build does not have; run the newer release, or restore the backup taken before the upgrade"
    )]
    SchemaTooNew {
        /// The version recorded in `_sqlx_migrations`.
        version: i64,
    },
    /// A commit failed with a fatal SQLite I/O error earlier in this process.
    /// Every catalog call is refused until a restart (D-R-6).
    #[error("catalog stopped after an I/O error; restart required")]
    Stopped,
    /// The storage shut down and closed `catalog.db`.
    #[error("catalog closed for shutdown")]
    Closed,
}

/// One staged-object collection failed.
///
/// [`GcError::Object`] names the key. Keys contain project ids, so callers log
/// this error at debug.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// `catalog.db` rejected the read or the row delete.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The object store rejected the delete.
    #[error(transparent)]
    Object(#[from] ObjectError),
    /// A test fault armed at a GC hook point.
    #[error("garbage collection was interrupted")]
    Fault,
}

impl GcError {
    /// The variant name in snake case. GC logs print this instead of the error
    /// text: [`GcError::Object`] displays a staged path, and staged rows include
    /// media paths, which hold the blob's hash (spec 03 §9).
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Catalog(_) => "catalog",
            Self::Object(_) => "object",
            Self::Fault => "fault",
        }
    }
}

impl CatalogError {
    pub(crate) fn db(source: sqlx::Error) -> Self {
        Self::Db { source }
    }

    pub(crate) fn corrupt(what: &'static str) -> Self {
        Self::Corrupt { what }
    }
}

/// A catalog commit failed.
///
/// A conflict leaves the staged objects in place. After either variant the
/// in-memory catalog is unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// A precondition failed and the transaction rolled back.
    #[error("catalog conflict: {reason}")]
    Conflict {
        /// Static reason. Never a path or a cell value.
        reason: &'static str,
    },
    /// SQLite failed, or an injected `CatalogCommit` fault fired.
    #[error("catalog database: {source}")]
    Db {
        /// The SQLite error, or the injected fault wrapped as I/O.
        #[source]
        source: sqlx::Error,
    },
    /// A commit failed with a fatal SQLite I/O error earlier in this process.
    /// Every catalog call is refused until a restart (D-R-6).
    #[error("catalog stopped after an I/O error; restart required")]
    Stopped,
    /// The storage shut down and closed `catalog.db`.
    #[error("catalog closed for shutdown")]
    Closed,
}

impl CommitError {
    pub(crate) fn db(source: sqlx::Error) -> Self {
        Self::Db { source }
    }

    /// The committer refused the call: [`CatalogError::Closed`] after shutdown,
    /// [`CatalogError::Stopped`] (the only other refusal) after a fatal I/O error.
    pub(crate) fn refused(error: &CatalogError) -> Self {
        match error {
            CatalogError::Closed => Self::Closed,
            _ => Self::Stopped,
        }
    }

    pub(crate) fn conflict(reason: &'static str) -> Self {
        Self::Conflict { reason }
    }
}

/// `register_deletion` or `deletion_status` failed.
#[derive(Debug, thiserror::Error)]
pub enum DeletionError {
    /// The request is malformed. `reason` is static and never holds an id from the scope.
    #[error("invalid deletion: {reason}")]
    Invalid {
        /// Static reason. Never a trace id, user id or session id.
        reason: &'static str,
    },
    /// No deletion with this id was registered.
    #[error("deletion not found")]
    NotFound,
    /// `catalog.db` failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// `put_media`, `get_media` or a media collection failed (spec 03 §9).
///
/// [`MediaError::Object`] names the object key, which holds the blob's hash:
/// media code logs [`MediaError::label`] instead of the error text.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The blob is malformed. `reason` is static.
    #[error("invalid media: {reason}")]
    Invalid {
        /// Static reason. Never blob content.
        reason: &'static str,
    },
    /// The given hash does not match the bytes.
    #[error("media hash does not match its bytes")]
    HashMismatch,
    /// No `media` row for this hash.
    #[error("media not found")]
    NotFound,
    /// The requested byte range is outside the blob.
    #[error("media range is outside the blob")]
    InvalidRange,
    /// The object store rejected the read or the write.
    #[error(transparent)]
    Object(#[from] ObjectError),
    /// `catalog.db` failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// Reading the live set failed.
    #[error("media query: {0}")]
    Query(#[from] EngineError),
    /// An injected test fault.
    #[error("injected fault")]
    Fault,
    /// The project is dropped (spec 03 §7).
    #[error("project is dropped")]
    ProjectDropped,
    /// Free disk is under `storage.min_free_bytes` (spec 03 §10). The server maps it to 503.
    #[error("free disk is under storage.min_free_bytes")]
    DiskFull,
}

impl MediaError {
    /// The variant name in snake case. Media logs print this instead of the
    /// error text: [`MediaError::Object`] displays an object path, which holds
    /// the blob's hash (spec 03 §9).
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "invalid",
            Self::HashMismatch => "hash_mismatch",
            Self::NotFound => "not_found",
            Self::InvalidRange => "invalid_range",
            Self::Object(_) => "object",
            Self::Catalog(_) => "catalog",
            Self::Query(_) => "query",
            Self::Fault => "fault",
            Self::ProjectDropped => "project_dropped",
            Self::DiskFull => "disk_full",
        }
    }
}

/// An object-store read, write, or delete failed.
///
/// `path` is an object key or a filesystem path. The message never includes
/// object bytes.
#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    /// A local filesystem call failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being done, for example `writing object`.
        action: &'static str,
        /// Filesystem path involved in the failed call.
        path: PathBuf,
        /// The filesystem error.
        #[source]
        source: io::Error,
    },
    /// The final object name is already present.
    ///
    /// Names are unique, so this is a caller bug or a retried upload that
    /// actually landed.
    #[error("object already exists: {path}")]
    AlreadyExists {
        /// Relative object key.
        path: String,
    },
    /// A local fsync failed earlier in this process. Local object writes and
    /// deletes are refused until a restart (D-R-6).
    #[error("local object store stopped after a failed fsync; restart required")]
    SyncFailed,
    /// A remote `put` or multipart upload failed.
    ///
    /// The object may already exist: a failed completion is ambiguous. The
    /// flush retries under a new name and leaves this key for GC.
    #[error("object upload {path}: {source}")]
    Upload {
        /// Object key that was being written.
        path: String,
        /// The object-store error.
        #[source]
        source: Box<object_store::Error>,
    },
    /// A store operation other than an upload failed.
    #[error("object store {action} {path}: {source}")]
    Store {
        /// What was being done, for example `deleting object`.
        action: &'static str,
        /// Object key.
        path: String,
        /// The object-store error.
        #[source]
        source: Box<object_store::Error>,
    },
    /// The object key is not a relative path of normal segments.
    #[error("invalid object path")]
    InvalidPath,
    /// The S3 client or the local store could not be built.
    #[error("object store client: {source}")]
    Client {
        /// The object-store error. It does not include credentials.
        #[source]
        source: Box<object_store::Error>,
    },
    /// Object deletion is off (restore mode, D-N-34). Every deletion is refused, and
    /// the caller keeps its catalog row for a later server to retry.
    #[error("object deletion is off (restore mode)")]
    DeletionOff,
}

impl ObjectError {
    /// The variant name in snake case. Logs and [`OrphanError::Delete`] use it
    /// instead of the error text, which names the object key.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::AlreadyExists { .. } => "already_exists",
            Self::Upload { .. } => "upload",
            Self::Store { .. } => "store",
            Self::InvalidPath => "invalid_path",
            Self::Client { .. } => "client",
            Self::SyncFailed => "sync_failed",
            Self::DeletionOff => "deletion_off",
        }
    }

    pub(crate) fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn upload(path: &str, source: object_store::Error) -> Self {
        Self::Upload {
            path: path.to_owned(),
            source: Box::new(source),
        }
    }

    pub(crate) fn store(action: &'static str, path: &str, source: object_store::Error) -> Self {
        Self::Store {
            action,
            path: path.to_owned(),
            source: Box::new(source),
        }
    }

    pub(crate) fn client(source: object_store::Error) -> Self {
        Self::Client {
            source: Box::new(source),
        }
    }
}

/// The object store did not answer a probe (`Storage::probe_objects`).
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Neither the HEAD nor the PUT of the marker succeeded. The text names no key and no host.
    #[error("the object store is unreachable")]
    Unreachable,
}

/// Encoding a partition as one Parquet file failed.
///
/// The message names the table or a static reason. It never includes row content.
#[derive(Debug, thiserror::Error)]
pub enum ParquetWriteError {
    /// The batches cannot be written: empty, the wrong schema, or a bad level.
    #[error("parquet write: {reason}")]
    Invalid {
        /// Static reason. Never a cell value.
        reason: &'static str,
    },
    /// Columnar batch preparation or encoding failed.
    #[error("parquet write: {source}")]
    Column {
        /// The columnar error.
        #[source]
        source: ColumnError,
    },
    /// The Parquet writer failed.
    #[error("parquet write: {source}")]
    Parquet {
        /// The Parquet error.
        #[source]
        source: Box<parquet::errors::ParquetError>,
    },
}

/// The trace anchor could not partition rows or rebuild its state.
#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    /// A required column of a spans or traces batch is missing or mistyped.
    #[error("anchor: missing or invalid column {name}")]
    Column {
        /// Column name. Never a row value.
        name: &'static str,
    },
    /// Columnar batch processing failed.
    #[error("anchor: {0}")]
    Batch(#[from] ColumnError),
    /// An async Parquet read during the startup rebuild failed.
    #[error("anchor rebuild: {0}")]
    Parquet(#[source] Box<parquet::errors::ParquetError>),
}

impl AnchorError {
    pub(crate) fn column(name: &'static str) -> Self {
        Self::Column { name }
    }

    pub(crate) fn parquet(source: parquet::errors::ParquetError) -> Self {
        Self::Parquet(Box::new(source))
    }

    pub(crate) fn from_column(err: ColumnError) -> Self {
        Self::Batch(err)
    }

    pub(crate) fn is_store_error(&self) -> bool {
        matches!(self, Self::Parquet(err) if crate::verify::is_store_error(err))
    }
}

/// Building or merging trace partials failed.
#[derive(Debug, thiserror::Error)]
pub enum PartialsError {
    /// A required column is missing, has the wrong type, or holds an invalid id.
    #[error("partials column {name} is missing or invalid")]
    Column {
        /// The `traces` or `spans` column name. Never a row value.
        name: String,
    },
    /// A `trace_id` value is not a valid id.
    #[error("partials trace id is invalid")]
    InvalidTraceId,
    /// Columnar batch construction failed.
    #[error("partials: {0}")]
    Batch(#[from] ColumnError),
}

impl PartialsError {
    pub(crate) fn column(name: &str) -> Self {
        Self::Column {
            name: name.to_owned(),
        }
    }

    pub(crate) fn from_column(err: ColumnError) -> Self {
        Self::Batch(err)
    }
}

/// A compaction job or pass failed. No variant carries row content.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    /// Reading or merging the input files failed.
    #[error("reading compaction inputs failed")]
    Read {
        /// Opaque internal engine error. Names category only, never values.
        #[source]
        source: EngineError,
    },
    /// Encoding an output failed.
    #[error(transparent)]
    Write(#[from] ParquetWriteError),
    /// Writing, publishing or deleting an object failed.
    #[error(transparent)]
    Object(#[from] ObjectError),
    /// The catalog refused or failed the edit, or a catalog read failed.
    #[error(transparent)]
    Commit(#[from] CommitError),
    /// Recording the output paths in `staged_objects` failed (`Committer::stage`).
    #[error(transparent)]
    Stage(#[from] CatalogError),
    /// Duplicate collapsing failed on corrupt input.
    #[error(transparent)]
    Dedup(#[from] DedupError),
    /// Computing trace partials failed.
    #[error(transparent)]
    Partials(#[from] PartialsError),
    /// The job's inputs break an invariant it relies on.
    #[error("compaction input: {reason}")]
    Input {
        /// Static reason. Never a cell value.
        reason: &'static str,
    },
    /// A blocking task panicked or was cancelled.
    #[error("compaction task failed")]
    Task,
    /// A test hook failed the job.
    #[error("compaction stopped by a test hook")]
    Fault,
    /// The storage is shutting down; the job gave up before committing.
    #[error("compaction stopped for shutdown")]
    Stopped,
    /// The covered-row filter failed while merging or checking inputs.
    #[error("deletion filter: {reason}")]
    Deletion {
        /// Static reason. Never a cell value.
        reason: &'static str,
    },
    /// Free disk minus the other jobs' reservations is less than the job's
    /// inputs. A later pass retries the job (spec 03 §5, §10).
    #[error("not enough free disk for the job")]
    DiskFull,
}

impl From<cetra_query::QueryError> for CompactionError {
    fn from(err: cetra_query::QueryError) -> Self {
        Self::Read {
            source: EngineError::from_query(&err),
        }
    }
}

impl From<ColumnError> for CompactionError {
    fn from(err: ColumnError) -> Self {
        Self::Read {
            source: EngineError::from_column(&err),
        }
    }
}

impl CompactionError {
    /// The variant name in snake case. The job failure logs print this instead
    /// of the error text: internal read errors never quote predicates or values (spec 03 §7).
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write(_) => "write",
            Self::Object(_) => "object",
            Self::Commit(_) => "commit",
            Self::Stage(_) => "stage",
            Self::Dedup(_) => "dedup",
            Self::Partials(_) => "partials",
            Self::Input { .. } => "input",
            Self::Task => "task",
            Self::Fault => "fault",
            Self::Stopped => "stopped",
            Self::Deletion { .. } => "deletion",
            Self::DiskFull => "disk_full",
        }
    }

    /// For [`Self::Read`], the engine error category in snake case; `None` otherwise.
    /// Never exposes query text or row literals (spec 03 §7).
    pub(crate) fn read_source(&self) -> Option<&'static str> {
        let Self::Read { source } = self else {
            return None;
        };
        Some(source.kind_label())
    }
}

/// Why [`crate::Storage::snapshot_catalog`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The snapshot target already exists.
    #[error("the snapshot target already exists")]
    Exists,
    /// The snapshot target is not valid UTF-8.
    #[error("the snapshot target is not valid UTF-8")]
    NonUtf8Path,
    /// SQLite failed, or a value of the copy is invalid.
    #[error("catalog snapshot: {0}")]
    Db(#[from] sqlx::Error),
    /// The catalog connection is closed (the storage shut down).
    #[error("catalog snapshot: {0}")]
    Catalog(#[from] CatalogError),
    /// A `media` row of the copy names a project id that does not parse.
    #[error("catalog snapshot: invalid project id in the copy")]
    BadProjectId,
}

/// Why [`crate::Storage::copy_objects`] failed.
#[derive(Debug, thiserror::Error)]
pub enum CopyObjectsError {
    /// The objects are in a remote store, not on this machine.
    #[error("objects are in a remote store; back up the bucket instead")]
    RemoteStore,
    /// Object `key_index` of the snapshot is missing, or could not be linked or copied.
    #[error("object {key_index} of the snapshot: {source}")]
    Io {
        /// Position of the object in the snapshot's list. Never the path.
        key_index: usize,
        /// The filesystem error.
        source: io::Error,
    },
    /// Creating or syncing the destination directory failed.
    #[error("backup directory: {source}")]
    Dir {
        /// The filesystem error.
        source: io::Error,
    },
    /// The blocking copy task panicked.
    #[error("copy task panicked")]
    Task,
    /// The copy was cancelled before it finished: its caller gave up on it.
    #[error("copy cancelled")]
    Cancelled,
}

/// Why [`crate::Maintenance::collect_orphans`] failed.
#[derive(Debug, thiserror::Error)]
pub enum OrphanError {
    /// Listing the object store failed.
    #[error("listing the object store: {0}")]
    List(object_store::Error),
    /// Deleting an orphan failed. Holds the [`ObjectError::label`] only: the error
    /// text names the object key, which holds the blob's hash (spec 03 §9).
    #[error("deleting an orphan: {0}")]
    Delete(&'static str),
    /// Reading the catalog failed.
    #[error("reading the catalog: {0}")]
    Db(#[from] sqlx::Error),
    /// The catalog connection is closed (the storage shut down).
    #[error("reading the catalog: {0}")]
    Catalog(#[from] CatalogError),
    /// A `media` row names a project id that does not parse.
    #[error("reading the catalog: invalid project id")]
    BadProjectId,
}

impl OrphanError {
    /// The delete failure of one orphan, keeping only the object error's label.
    pub(crate) fn delete(error: &ObjectError) -> Self {
        Self::Delete(error.label())
    }
}

#[cfg(test)]
mod tests {
    use super::{CompactionError, EngineError, ObjectError, OrphanError, engine_error_for_test};

    #[test]
    fn a_read_failure_names_its_category_and_hides_arbitrary_text() {
        let secret = engine_error_for_test("secret-literal");
        assert!(!secret.to_string().contains("secret-literal"));
        assert!(!format!("{secret:?}").contains("secret-literal"));
        let wrapped = CompactionError::Read {
            source: EngineError::from_resources_exhausted(),
        };
        assert_eq!(wrapped.read_source(), Some("resources_exhausted"));
        let io = CompactionError::Read {
            source: EngineError::from_io(),
        };
        assert_eq!(io.read_source(), Some("io"));
        assert_eq!(CompactionError::DiskFull.read_source(), None);
        assert_eq!(CompactionError::Task.read_source(), None);
    }

    #[test]
    fn an_orphan_delete_error_does_not_show_the_object_key() {
        let key = "media/0190f2a4-7c3d-7e21-9b8a-1234567890ab/9f86d081884c7d659a2feaa0c55ad015";
        let error = ObjectError::store(
            "deleting object",
            key,
            object_store::Error::Generic {
                store: "test",
                source: "refused".into(),
            },
        );
        let text = OrphanError::delete(&error).to_string();
        assert!(!text.contains(key), "{text}");
        assert!(!text.contains("9f86d081"), "{text}");
        assert_eq!(text, "deleting an orphan: store");
    }

    #[test]
    fn engine_error_preserves_corrupt_vs_store_vs_resources_exhausted() {
        use super::AnchorError;
        use crate::verify::may_be_corrupt;
        use cetra_query::QueryError;

        let too_large = EngineError::from_query(&QueryError::TooLarge);
        assert!(!may_be_corrupt(&too_large));
        assert_eq!(
            CompactionError::Read { source: too_large }.read_source(),
            Some("resources_exhausted")
        );

        let corrupt =
            EngineError::from_query(&QueryError::CorruptData("corrupt parquet file".to_owned()));
        assert!(may_be_corrupt(&corrupt));
        assert_eq!(
            CompactionError::Read { source: corrupt }.read_source(),
            Some("parquet")
        );

        let store =
            EngineError::from_query(&QueryError::StorageIo("storage read error".to_owned()));
        assert!(!may_be_corrupt(&store));
        assert_eq!(
            CompactionError::Read { source: store }.read_source(),
            Some("object_store")
        );

        let string_only = EngineError::from_query(&QueryError::internal("corrupt parquet file"));
        assert!(!may_be_corrupt(&string_only));
        assert_eq!(string_only.kind_label(), "internal");

        let anchor_store = AnchorError::parquet(parquet::errors::ParquetError::External(Box::new(
            object_store::Error::Generic {
                store: "test",
                source: "err".into(),
            },
        )));
        assert!(anchor_store.is_store_error());
        let anchor_missing = AnchorError::parquet(parquet::errors::ParquetError::External(
            Box::new(object_store::Error::NotFound {
                path: "x".to_owned(),
                source: "missing".into(),
            }),
        ));
        assert!(!anchor_missing.is_store_error());
    }
}

/// A batch could not be deduplicated. `reason` is a static description, never row content.
#[derive(Debug, thiserror::Error)]
#[error("dedup failed: {reason}")]
pub struct DedupError {
    /// Static reason. Never a cell value.
    reason: &'static str,
}

impl DedupError {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }
}

/// A memtable flush failed before the catalog commit.
///
/// The frozen memtable stays. The task retries with new file names.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FlushError {
    /// Sorting or encoding the frozen batches failed.
    #[error("flush prepare: {0}")]
    Prepare(ParquetWriteError),
    /// Anchor partitioning failed.
    #[error("flush anchor: {0}")]
    Anchor(#[from] AnchorError),
    /// Computing the trace partials of a spans file failed.
    #[error("flush partials: {0}")]
    Partials(PartialsError),
    /// A span identity column was missing or null.
    #[error("flush dedup: {reason}")]
    Dedup {
        /// Static reason. Never a cell value.
        reason: &'static str,
    },
    /// The covered-row filter failed while preparing the flush.
    #[error("deletion filter: {reason}")]
    Deletion {
        /// Static reason. Never a cell value.
        reason: &'static str,
    },
    /// Recording the paths in `staged_objects` failed.
    #[error("flush stage: {0}")]
    Stage(CatalogError),
    /// A durable object write failed. The object may already exist.
    #[error("flush write: {0}")]
    Write(ObjectError),
    /// The catalog edit failed. Staged objects stay for GC.
    #[error("flush commit: {0}")]
    Commit(CommitError),
    /// `spawn_blocking` or an upload task panicked.
    #[error("flush task panicked")]
    Task,
    /// A test hook returned a fault.
    #[error("flush fault")]
    Fault,
}

impl FlushError {
    pub(crate) fn dedup(reason: &'static str) -> Self {
        Self::Dedup { reason }
    }

    pub(crate) fn prepare(source: ParquetWriteError) -> Self {
        Self::Prepare(source)
    }

    pub(crate) fn partials(source: PartialsError) -> Self {
        Self::Partials(source)
    }

    pub(crate) fn stage(source: CatalogError) -> Self {
        Self::Stage(source)
    }

    pub(crate) fn write(source: ObjectError) -> Self {
        Self::Write(source)
    }

    pub(crate) fn commit(source: CommitError) -> Self {
        Self::Commit(source)
    }
}

impl ParquetWriteError {
    pub(crate) fn invalid(reason: &'static str) -> Self {
        Self::Invalid { reason }
    }

    pub(crate) fn parquet(source: parquet::errors::ParquetError) -> Self {
        Self::Parquet {
            source: Box::new(source),
        }
    }

    pub(crate) fn from_column(err: ColumnError) -> Self {
        Self::Column { source: err }
    }
}
