# B5 — `Storage::collect_orphans`

Read [00-design.md](00-design.md) first, especially § Design › Restore (D-N-21).

**Goal:** `Storage::collect_orphans(min_age, dry_run)` lists the object store under `tables/` and
`media/`, and does two things:
- deletes, unless `dry_run`, every object the catalog does not know that is older than `min_age`;
- reports every object the catalog needs that the store does not have.

The CLI runs it with the server stopped (B8), after a restore or a crash.

**Edit only:** `crates/storage/src/gc.rs`, `crates/storage/src/catalog/committer.rs`,
`crates/storage/src/lib.rs`, `crates/storage/src/error.rs`, `crates/storage/src/objects.rs` (one
visibility change).

**Needs:** B3 (`object_gc_gate`).

**Commit:** `Add Storage::collect_orphans`

## Read first

- `gc.rs`: the module doc ("the bucket is never listed" is about the periodic GC; this is an explicit
  operator command), `collect_once`.
- `objects.rs`: `raw_store()`, `is_temp_name`, `media_path`, `table_file_path`.
- `committer.rs`: how other methods query `inner.connection`.

## Changes

### `committer.rs`

```rust
/// Every store key the catalog knows: `data_files.path` (any state), media keys,
/// and `staged_objects.path`; and the keys it needs: active data files and
/// media of projects not dropping or dropped.
pub(crate) async fn known_and_needed_keys(&self) -> Result<(HashSet<String>, HashSet<String>), sqlx::Error>
```

- "Needed":
  - `SELECT path FROM data_files WHERE state = 'active'`;
  - `SELECT m.project_id, m.sha256 FROM media m WHERE NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = m.project_id AND p.state IN ('dropping', 'dropped'))`,
    with the key `Objects::media_path(project, &sha256)`.
- "Known":
  - `SELECT path FROM data_files` (every state: active, obsolete, quarantined);
  - every `media` row's key (all projects);
  - `SELECT path FROM staged_objects`.

Run the queries on `inner.connection` under the committer lock.

### `objects.rs`

`fn is_temp_name(name: &OsStr) -> bool` becomes `pub(crate)`. Callers pass the key's last segment as
`OsStr::new(segment)`.

### `gc.rs`

```rust
/// What [`crate::Storage::collect_orphans`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReport {
    /// Unknown objects older than `min_age` (deleted unless `dry_run`).
    pub orphans: u64,
    pub orphan_bytes: u64,
    /// Unknown objects younger than `min_age`, left alone.
    pub recent_unknown: u64,
    /// Data files the catalog needs that the store does not have.
    pub missing: u64,
    /// Media blobs the catalog references that the store does not have (a warning:
    /// media follows its own deletion rule, see N M2's design § Backup layout).
    pub missing_media: u64,
    pub dry_run: bool,
}
```

Re-export `OrphanReport` from `lib.rs`.

### `lib.rs`

```rust
/// Lists the object store and deletes objects the catalog does not know that
/// are older than `min_age` (unless `dry_run`), and counts objects the catalog
/// needs that are missing (N M2, D-N-21). For an operator after a restore or a
/// crash; the periodic GC never lists the store.
///
/// # Errors
///
/// [`OrphanError`] when listing, deleting or reading the catalog fails.
pub async fn collect_orphans(
    &self,
    min_age: std::time::Duration,
    dry_run: bool,
) -> Result<OrphanReport, OrphanError>
```

Algorithm:

1. Take `self.committer.object_gc_gate` for write, so no object deletion runs concurrently.
2. Read `needed` **before** listing, and `known` **after** listing. An object published while the
   list runs is then in `known` (or staged), and never counted as an orphan. Split
   `known_and_needed_keys` into `needed_keys()` and `known_keys()`.
3. For each prefix, `"tables"` then `"media"`, stream `self.objects.raw_store().list(Some(&prefix))`.
   For each `ObjectMeta`:
   1. `key = meta.location.as_ref()`. Skip it if its last segment is a temporary name
      (`is_temp_name`).
   2. Remove it from a working copy of `needed`.
   3. If `known` contains it, continue.
   4. If `now − meta.last_modified < min_age`, count `recent_unknown` and continue.
   5. Count `orphans` and `orphan_bytes += meta.size`. Unless `dry_run`, `self.objects.delete(key)`.
4. What is left of the working copy of `needed` gives the two counts: keys under `tables/` go to
   `missing`, keys under `media/` go to `missing_media`.
5. Log one `info` line with the four counts (no keys), and return the report.

`now` comes from `std::time::SystemTime::now()`, compared with `meta.last_modified` (a
`chrono::DateTime<Utc>`). This is wall-clock time on purpose: object timestamps are wall-clock.

### `error.rs`

```rust
/// Why [`crate::Storage::collect_orphans`] failed.
#[derive(Debug, thiserror::Error)]
pub enum OrphanError {
    #[error("listing the object store: {0}")]
    List(object_store::Error),
    #[error("deleting an orphan: {0}")]
    Delete(#[from] crate::error::ObjectError),
    #[error("reading the catalog: {0}")]
    Db(#[from] sqlx::Error),
    #[error("reading the catalog: invalid project id")]
    BadProjectId,
}
```

`Objects::delete` returns `ObjectError`. Its `Display` names the key, so `OrphanError` keeps only
its label (spec 03 §9), as `media.rs` and `apply_deletions` log it.

## Tests

Three traps:
- `Storage::open` already removes temporary files and runs one GC pass, so write test objects
  **after** opening.
- `raw_store()` takes `object_store::path::Path` (`ObjectPath::from(key)`), not `&str`.
- End every test with `storage.shutdown().await`, as the `gc.rs` tests do.

The tests call `collect_orphans` on a fully started engine. The CLI (B8) must not: `Storage::open`
runs the staged-object GC, obsolete GC and `finish_drops`, and starts compaction, so a dry run would
delete objects and rewrite the restored `catalog.db` (review N-2). B8 uses the maintenance open
(`storage::Maintenance`, below the fixes of the full plan review): it takes `LOCK`, migrates
`catalog.db`, and starts no task and no GC. A `--min-age` below an hour is safe only because the
server is stopped (`LOCK`) and nothing appends.


- `collect_orphans_deletes_unknown_old_objects`:
  1. Write an object directly with `raw_store().put` at `tables/spans/project=…/day=2026-01-01/0-<uuid>.parquet`.
     It is unknown.
  2. With `min_age = 0`, the report has `orphans = 1`, and the object is gone.
  3. Every catalog file still exists.
- `collect_orphans_dry_run_deletes_nothing`: same setup with `dry_run = true`: `orphans = 1` and the
  object still exists.
- `collect_orphans_leaves_recent_unknown_objects`: with `min_age = 1h`, a just-written unknown object
  counts as `recent_unknown = 1` and is kept.
- `collect_orphans_reports_missing_needed_objects`: append and flush, so an active `data_files` row
  exists, then delete its object directly through `raw_store().delete`. The report has `missing = 1` and `orphans = 0`.
- `collect_orphans_keeps_staged_and_obsolete_objects`:
  1. A compaction obsoletes files (not yet collected), and a staged row exists.
  2. With `min_age = 0`, neither is deleted.
- `collect_orphans_reports_missing_media`: delete a live media blob directly; the report has
  `missing_media = 1` and `missing = 0`.
- `collect_orphans_keeps_dropping_project_media`: a project in `dropping` state with a media row
  (known, not needed). The object is neither deleted nor counted as missing.
- `collect_orphans_skips_temp_names`: a file named like a temporary upload (see `is_temp_name`) is
  neither counted nor deleted.

## Verify

```bash
cargo test -p llmobs-storage --all-features collect_orphans
just check
```

## Done when

- [ ] `Storage::collect_orphans`, `OrphanReport` and `OrphanError` exist with the eight tests.
- [ ] `just check` passes. One commit: `Add Storage::collect_orphans`.
