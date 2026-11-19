# B3 — Catalog snapshot and the object GC gate

Read [00-design.md](00-design.md) first, especially § What exists › Object deletion and § Design ›
Consistency (D-N-18).

**Goal:** `Storage::snapshot_catalog(path)` writes a transactionally consistent copy of `catalog.db`
to `path` and returns a `CatalogSnapshot`. While the snapshot is alive, no object it lists can be
deleted:
- data files, through the existing `retired` reference count, because the snapshot holds a clone of
  the `Arc<Catalog>`;
- media, through a new `object_gc_gate` that the media deletions take for read.

**Edit only:** `crates/storage/src/catalog/committer.rs`, `crates/storage/src/media.rs`,
`crates/storage/src/gc.rs`,
`crates/storage/src/lib.rs`, `crates/storage/src/error.rs`, and a new `crates/storage/src/snapshot.rs`.

**Commit:** `Add catalog snapshots for online backups`

## Read first

- `catalog/committer.rs`: `Committer`, `CommitterInner` (`connection`, `catalog`, `retired`),
  `collect_obsolete_file` (the `Arc::strong_count` check), `collect_media`, `media_of`,
  `delete_media_row`.
- `media.rs`: `MediaGc::collect`, `MediaGc::finish_drop`.
- `objects.rs`: `Objects::media_path`.
- `migrations/catalog/0001_catalog.sql`, `0005_media.sql`, `0006_projects.sql`: the tables
  `data_files` (`path`, `state`), `media` (`project_id`, `sha256`, `size_bytes`) and `projects`
  (`project_id`, `state`).

## Lock order

Always take `object_gc_gate` **before** the committer's `inner` mutex, never while holding it.
- Media deletion takes the gate for read, then calls the committer.
- The snapshot takes the gate for write, then the committer lock.

A deletion holding the committer lock never waits for the gate, so there is no deadlock.

Commits wait only for the `VACUUM INTO` itself, which runs under the committer lock. That is a known
cost (00-design § Consistency): it grows with the `data_files` and `media` rows, and on a large
catalog flushes stall for its duration.

One consequence to document: tokio's `RwLock` prefers writers. While a snapshot is alive, a
`drop_project` waits in `finish_drop` until the backup ends, and media GC waits too. Neither is a
deadlock.

## Changes

### `committer.rs`

1. Add a public-in-crate field to `Committer`, initialised in `Committer::open`:

   ```rust
   /// Held for read by every object deletion and for write by a catalog snapshot
   /// (N M2, D-N-18). Take it before `inner`, never while holding `inner`.
   pub(crate) object_gc_gate: Arc<tokio::sync::RwLock<()>>,
   ```

2. Add a method:

   ```rust
   /// `VACUUM INTO path` on the catalog connection and a clone of the in-memory
   /// catalog, in one critical section: the copy and the clone describe the same
   /// commit. `path` must not exist.
   pub(crate) async fn snapshot_into(&self, path: &Path) -> Result<Arc<Catalog>, sqlx::Error>
   ```

   It locks `inner`, runs `sqlx::query("VACUUM INTO ?").bind(path_str)` on `inner.connection`, clones
   `inner.catalog`, and releases the lock. `path_str` is `path.to_str()`; a non-UTF-8 path is mapped to
   `SnapshotError::NonUtf8Path` by the caller before this call.

### `media.rs`

- In `MediaGc::collect`, compute the live set first. That is a scan, and the gate must not be held
  during it. Then take `self.committer.object_gc_gate.read().await` just before calling
  `committer.collect_media`, and hold it until that call returns.
- In `MediaGc::finish_drop`, take the same read guard before the `for` loop and hold it to the end.

In both, the guard is taken before any committer call (§ Lock order).

### `gc.rs`

In `collect_obsolete`, take `committer.object_gc_gate.read().await` around each call to
`Committer::collect_obsolete_file`, outside the committer lock. Files the verify pass quarantines
during a backup leave the catalog without entering `retired`. The `Arc` pin does not cover them; the
gate does.

### `snapshot.rs` (new; `mod snapshot;` in `lib.rs`, `pub use snapshot::{CatalogSnapshot, LiveObject};`)

```rust
/// A consistent copy of `catalog.db` and the pin that keeps its objects (N M2,
/// D-N-18). Dropping it releases the pin.
pub struct CatalogSnapshot {
    /// The copied file.
    pub path: PathBuf,
    /// Every object the copy needs: active data files and live media.
    pub objects: Vec<LiveObject>,
    /// `table_state.flushed_seq` of the copy: `[spans, scores]`.
    pub flushed_seq: [u64; 2],
    _catalog: Arc<Catalog>,
    _gate: tokio::sync::OwnedRwLockWriteGuard<()>,
}

/// One object a snapshot needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveObject {
    /// Store-relative key: `tables/…/….parquet` or `media/{project}/{sha256}`.
    pub key: String,
    pub size_bytes: u64,
}
```

- `CatalogSnapshot` implements `Debug` by hand, showing `path`, the object count and `flushed_seq`.
- The snapshot takes the gate with `Arc::clone(&gate).write_owned()`, to hold the guard in the struct.
- Re-export `SnapshotError`, `CatalogSnapshot` and `LiveObject` from `lib.rs`.
- `Storage::drop_project` must not wait for a whole backup. It calls `finish_drop` only if
  `object_gc_gate.try_read()` succeeds; while a backup holds the gate, it returns after its commit and
  leaves the media to `finish_drops`, which the GC task retries every 10 minutes.

### `lib.rs`

```rust
/// Writes a consistent copy of `catalog.db` to `path` and pins every object it
/// lists until the returned [`CatalogSnapshot`] is dropped (N M2, D-N-18).
///
/// # Errors
///
/// [`SnapshotError`] when `path` exists or is not UTF-8, or when SQLite fails.
pub async fn snapshot_catalog(&self, path: &Path) -> Result<CatalogSnapshot, SnapshotError>
```

Algorithm:

1. If `path` exists, return `SnapshotError::Exists`. If it is not UTF-8, return
   `SnapshotError::NonUtf8Path`.
2. `let gate = Arc::clone(&self.committer.object_gc_gate).write_owned().await;`
3. `let catalog = self.committer.snapshot_into(path).await?;`
4. Open the copy read-only, with a plain `SqliteConnectOptions::new().filename(path).read_only(true)`
   connection, and read:
   - `SELECT path, size_bytes FROM data_files WHERE state = 'active'`;
   - `SELECT m.project_id, m.sha256, m.size_bytes FROM media m WHERE NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = m.project_id AND p.state IN ('dropping', 'dropped'))`.
     The media key is `Objects::media_path(project, &sha256)`; parse `project_id` with the crate's
     usual `ProjectId` parser;
   - `SELECT table_name, flushed_seq FROM table_state`, mapped to `[spans, scores]`. A missing row is
     0.

   Close the connection.
5. Return the `CatalogSnapshot`.

### `error.rs`

```rust
/// Why [`crate::Storage::snapshot_catalog`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("the snapshot target already exists")]
    Exists,
    #[error("the snapshot target is not valid UTF-8")]
    NonUtf8Path,
    #[error("catalog snapshot: {0}")]
    Db(#[from] sqlx::Error),
    #[error("catalog snapshot: invalid project id in the copy")]
    BadProjectId,
}
```

## Tests (`snapshot.rs` or `lib.rs` test module)

- `snapshot_lists_active_files_and_media`:
  1. Append spans with a media blob (`put_media`).
  2. Get the rows flushed. Use `flush_now` if B2 is merged; otherwise use the pattern the `lib.rs`
     tests use: `config.flush.max_age = Duration::from_millis(100)`, then wait until the catalog
     lists files.
  3. Take a snapshot.
  4. The objects include every active data file of the in-memory catalog (compare with
     `published.load()`) and the media key.
  5. `flushed_seq` equals `committer.flushed_seq`.
  6. The copied file opens and has the same `data_files` count.
- `snapshot_holds_obsolete_gc`:
  1. A compaction obsoletes some files.
  2. Take a snapshot, then spawn `collect_obsolete(Duration::ZERO)`.
  3. After 200 ms the task is pending and every snapshot key exists.
  4. Drop the snapshot: the task finishes and the obsoleted files are gone.

  Never call `collect_obsolete` on the test's own task while it holds a snapshot: that deadlocks.
- `snapshot_defers_dropped_project_media`:
  1. With a snapshot held, `drop_project` returns promptly, and the project's media object still
     exists.
  2. Drop the snapshot and run the GC entry point that calls `finish_drops`: the object is gone.
- `snapshot_does_not_block_appends_or_flushes`: while a snapshot is held, appends succeed, and a flush
  driven by a small `flush.max_age` still commits: the catalog lists new files within 5 s.
- `snapshot_refuses_an_existing_target`: a second snapshot to the same path returns `Exists`.

## Verify

```bash
cargo test -p cetra-storage --all-features snapshot
just check
```

## Done when

- [ ] `Storage::snapshot_catalog`, `CatalogSnapshot`, `LiveObject` and `SnapshotError` exist.
- [ ] Media deletions take the gate before any committer call.
- [ ] The five tests pass, and `just check` passes. One commit:
      `Add catalog snapshots for online backups`.
