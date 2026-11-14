# B6 — `server::backup::run`

Read [00-design.md](00-design.md) first, all of § Design › Consistency and § Design › Backup layout
and manifest (D-N-18, D-N-19, D-N-24).

**Goal:** a server function that takes an online backup of the running instance into a directory,
following the nine steps of § Consistency, and returns a `BackupReport`. B7 exposes it on the control
socket.

**Edit only:** `crates/server/src/backup.rs` (new), `crates/server/src/lib.rs` (`pub mod backup;`),
`crates/server/src/http/mod.rs` (one field in `AppInner`), `crates/server/src/serve.rs` (initialise
the field; add `pub(crate) fn state(&self) -> &AppState` on `Running` for the tests),
`crates/server/src/test_support/mod.rs` (initialise the field in its `AppInner` literal), and `crates/server/Cargo.toml` only if a dependency already in the workspace is missing
(`sha2`, `hex` and `chrono` are already workspace dependencies).

**Commit:** `Add online backups to the server`

**Needs:** B2, B3, B4.

## Read first

- `crates/server/src/http/mod.rs`: `AppState`, `AppInner` (`config`, `meta`, `storage`).
- `crates/server/src/meta/mod.rs`: `MetaDb::reader()`.
- `crates/server/src/crypto.rs`: `key_path(data_dir)`; how the key source (`Env` or file) is known.
- `crates/server/src/config.rs`: `ServerConfig::storage_config()`,
  `StorageConfig::object_store_url()`, `storage.gc.grace`.
- The storage API from B2–B4: `flush_now`, `snapshot_catalog`, `copy_objects`, `CatalogSnapshot`,
  `CopyReport`.

## Changes

### `AppInner`

Add `pub(crate) backup_slot: tokio::sync::Mutex<()>,` (the single backup slot). Initialise it in
`serve::bind` where `AppInner` is built, and in `test_support/mod.rs`'s `AppInner` literal.

### `backup.rs`

Until B7 calls it, `run` is dead code in non-test builds, so the module starts with
`#![cfg_attr(not(test), allow(dead_code))]`, as `crypto.rs` does. B7 removes that line.

```rust
//! Online backups of a running server (N M2, D-N-17–D-N-19, D-N-24).

/// What the client asked for.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BackupRequest {
    pub to: PathBuf,
    #[serde(default)]
    pub meta_only: bool,
}

/// What a finished backup reports (also the socket answer, B7).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BackupReport {
    pub dir: PathBuf,
    pub mode: BackupMode,            // serialised "full" / "meta-only"
    pub created_at: String,          // RFC 3339, nanoseconds, UTC
    pub objects: u64,
    pub object_bytes: u64,
    pub linked: u64,
    pub copied: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackupMode { Full, MetaOnly }

/// Why a backup failed. `code()` is the stable string the socket returns.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("another backup is running")]
    Busy,
    #[error("the target directory is not empty")]
    TargetNotEmpty,
    #[error("the target directory is inside the data directory")]
    TargetInsideDataDir,
    #[error("--meta-only needs S3-compatible object storage; a local-mode backup must include its objects")]
    MetaOnlyNeedsS3,
    #[error("flush: {0}")]
    Flush(#[from] llmobs_storage::FlushNowError),
    #[error("meta.db copy: {0}")]
    Meta(sqlx::Error),
    #[error("catalog snapshot: {0}")]
    Snapshot(#[from] llmobs_storage::SnapshotError),
    #[error("objects: {0}")]
    Objects(#[from] llmobs_storage::CopyObjectsError),
    #[error("writing the backup: {0}")]
    Io(#[from] std::io::Error),
}
```

`BackupError::code()` returns, in variant order: `busy`, `target_not_empty`,
`target_inside_data_dir`, `meta_only_needs_s3`, `flush_failed`, `meta_copy_failed`,
`snapshot_failed`, `objects_failed`, `io`.

```rust
/// Takes an online backup into `request.to` (§ Consistency of N M2's design).
pub(crate) async fn run(state: &AppState, request: BackupRequest) -> Result<BackupReport, BackupError>
```

Steps (each maps to § Design › Consistency):

1. `let _slot = state.backup_slot.try_lock().map_err(|_| BackupError::Busy)?;`
2. Decide the mode. S3 mode is `matches!(storage config object_store_url(), ObjectStoreUrl::S3 { .. })`.
   - Local mode: `meta_only` → `MetaOnlyNeedsS3`; otherwise `Full`.
   - S3 mode: `MetaOnly`, whatever `meta_only` says (D-N-24).
3. Target checks, through `tokio::fs`:
   - Canonicalise `data_dir`, and the target's parent (create the parent with `create_dir_all`
     first). If the canonical target starts with the canonical `data_dir`, return
     `TargetInsideDataDir`.
   - If the target exists and is a non-empty directory (or not a directory), return
     `TargetNotEmpty`. Otherwise create it with mode `0o700` (`std::os::unix::fs::DirBuilderExt`).
4. `state.storage.flush_now(Duration::from_secs(60)).await?`.
5. `VACUUM INTO` the target's `meta.db` on one connection from `state.meta.reader()`:
   `sqlx::query("VACUUM INTO ?").bind(path_str)`. Map errors to `Meta`.
6. `let snapshot = state.storage.snapshot_catalog(&dir.join("catalog.db")).await?;`
7. `Full` mode: `let copy = state.storage.copy_objects(&snapshot, &dir.join("objects")).await?;`
   `MetaOnly` mode: `CopyReport { count: snapshot.objects.len(), bytes: sum of size_bytes, linked: 0, copied: 0 }`.
8. If `state.config.encryption_key.is_none()` (the key is the file; this is the test `open_secrets`
   uses) and `crypto::key_path(data_dir)` exists, copy it to `dir/secret.key` and set mode `0o600`. Record
   `encryption_key = "file"`, otherwise `"environment"`.
9. Write `manifest.json` exactly as § Backup layout and manifest shows:
   - `format: 1`;
   - `llmobs_version: env!("CARGO_PKG_VERSION")`;
   - `created_at`: `chrono::Utc::now()` taken right after step 4 returns, in RFC 3339 with
     nanoseconds and `Z` (`to_rfc3339_opts(SecondsFormat::Nanos, true)`);
   - `mode`;
   - `object_store`: `"local"` or `"s3"`;
   - `encryption_key`;
   - `flushed_seq: {"spans": snapshot.flushed_seq[0], "scores": snapshot.flushed_seq[1]}`;
   - `gc_grace_secs`;
   - `restorable_until`: `created_at + gc.grace` for `MetaOnly`, `null` for `Full`;
   - `files`: `meta.db`, `catalog.db` and `secret.key` when present, each with `bytes` and the hex
     SHA-256 computed by reading the file;
   - `objects: {count, bytes}`.

   Write it to `manifest.json.tmp`, `sync_all`, rename it to `manifest.json`, then `sync_all` the
   directory.
10. Drop `snapshot`. Return the report. `duration_ms` is measured from step 1.

Log one `info` line at the end: `mode`, `objects`, `object_bytes`, `duration_ms`. Never log the
target path: it is operator data, and the CLI prints it.

## Tests (`backup.rs` test module)

Start a real server with the `serve::bind` test pattern: a temp data dir, and `config.init` fields
set as `serve.rs`'s tests do. Append a few spans with `running.state().storage.append_spans(...)`,
then call `backup::run(running.state(), …)`.

Put the manifest building in a pure function `manifest(…) -> serde_json::Value`, and unit-test both
branches: `meta-only` with `restorable_until` set, and `environment` key. That covers what a server
test cannot reach without S3.

- `a_full_backup_has_every_file_and_a_manifest`:
  - `meta.db`, `catalog.db`, `secret.key` and `manifest.json` exist, along with every object of the
    report.
  - The manifest's SHA-256s match the files.
  - `mode` is `"full"` and `restorable_until` is `null`.
- `a_full_backup_restores_with_its_data`:
  1. Take the backup, then shut the server down.
  2. Build a new data dir with `meta.db`, `catalog.db`, `secret.key` and `objects/` from the
     backup.
  3. Start a server on it. The appended rows read back through
     `running.state().storage.query(project, …)`, the same way the storage tests read rows.
- `a_second_backup_while_one_runs_is_busy`: hold `state.backup_slot` in the test, then `run` returns
  `Busy` (`code() == "busy"`).
- `the_target_must_be_empty_and_outside_the_data_dir`:
  - a non-empty target → `TargetNotEmpty`;
  - `data_dir/x` → `TargetInsideDataDir`.
- `meta_only_is_refused_in_local_mode`: `meta_only: true` on a local-mode server →
  `MetaOnlyNeedsS3`.
- `the_key_is_not_copied_when_it_comes_from_the_environment`: with `encryption_key` set in the config,
  there is no `secret.key` in the backup, and the manifest says `"environment"`.

S3 mode is covered by the storage tests (B3, B4) and by B8's manual check; it needs no server test
here.

## Verify

```bash
cargo test -p llmobs-server --all-features backup
just check
```

## Done when

- [ ] `backup::run`, `BackupRequest`, `BackupReport`, `BackupMode` and `BackupError` exist with the
      six tests.
- [ ] The manifest matches the design exactly.
- [ ] `just check` passes. One commit: `Add online backups to the server`.
