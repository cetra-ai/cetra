# 01: free disk, the object-store probe, counters and the deletion switch

Read [00-design.md](00-design.md) first, especially § Storage API needed from track R (D-N-31, D-N-34, D-N-36).

> **Gate.**
> - `crates/storage` is track R's area. R reviewed this phase on 2026-11-15, and this file includes its six changes. N implements it; R does not.
> - **When:** after R's stacked PRs (M5–M7) are merged into `main`, so R never rebases onto N. Check with the coordinator before starting. If one of R's branches is still open and adds `StorageConfig { … }` literals, add the new field to those literals in this phase only once they are on `main`.

**Goal:** four additions to `cetra_storage`. With the switch at its default, nothing changes behaviour.

1. `StorageHealth` gets the free-disk fields.
2. `Storage::probe_objects()` checks that the object store answers, with a marker object (D-N-36).
3. `Storage::stats()` returns monotonic counters plus the WAL size.
4. `StorageConfig::delete_objects` (default `true`, not a TOML key) makes `Objects` refuse every deletion when `false` (D-N-34).

**Edit only:**
- `crates/storage/src/lib.rs`, `disk.rs`, `objects.rs`, `media.rs`, `error.rs`, `config.rs`, `maintenance.rs`, `catalog/committer.rs`;
- a new `crates/storage/src/counters.rs`;
- the counter sites in `flush.rs`, `compaction/job.rs` and `gc.rs`;
- every `StorageConfig { … }` literal: about 66 on `main`. `grep -rn "StorageConfig {" crates/` finds them, including `crates/storage/tests/*.rs`, `model_tests/`, and **three in `crates/server`**: `src/ingest/fixtures_tests.rs`, `src/test_support/mod.rs` (two). Add only the field to those three.
- `docs/spec/03-storage.md` (§ 5 and § 10, one sentence or paragraph each).

**Commit:** `Expose disk, object-store probe and counters from storage`

**Needs:** R's stacked PRs merged (gate above).

**Keep out of R's harnesses.** Do not call `probe_objects` or `stats` from the model, stress or soak tests (`model_tests/`, `tests/soak.rs`, the stress test). They are tested by the unit tests below only.

## Read first

- `crates/storage/src/lib.rs`:
  - `StorageHealth` and `Storage::health` (near line 868);
  - the re-exports near line 78;
  - the `disk`, `objects` and `committer` fields, and `Storage::disk()` (near line 1244).
- `crates/storage/src/disk.rs`: `free()`, `low()`, the `min_free` field, and `set_free_for_test` (near line 160).
- `crates/storage/src/objects.rs`:
  - `Objects::new` (near line 101);
  - `raw_store()`;
  - `delete` (near line 298) and `delete_len` (near line 368).
- `crates/storage/src/media.rs`: `MediaGc::finish_drop` (near line 176).
- `crates/storage/src/maintenance.rs`: how `Maintenance::open` builds `Objects`.
- `crates/storage/src/fs.rs`: `read_dir` and `metadata` (near lines 302 and 307). Every filesystem call in storage goes through this module (R's `fs` seam).
- `crates/storage/src/flush.rs` `run()`; `compaction/job.rs` `Compactor::run` (near line 301) and the pass loop (lines 210–280); `gc.rs` near lines 174 and 254.
- `crates/storage/src/test_support/chaos.rs`: `ChaosStore`, `Call::Head` and its faults.

## Changes

### 1. `StorageHealth`, free disk

Add three fields to `StorageHealth`, with doc comments:

```rust
/// Free disk is under `storage.min_free_bytes`: ingestion is refused (spec 03 §10).
pub disk_low: bool,
/// The least free space of the data directory and the local object root, from the last
/// measurement (at open, then every second).
pub disk_free_bytes: u64,
/// `storage.min_free_bytes`.
pub disk_min_free_bytes: u64,
```

- Add `pub(crate) fn min_free(&self) -> u64` to `Disk`.
- `Storage::health` fills the three fields from `self.disk.low()`, `self.disk.free()` and `self.disk.min_free()`.
- No new `statvfs` call, and no new measurement.

### 2. `Storage::probe_objects` (D-N-36)

In `error.rs`:

```rust
/// The object store did not answer a probe (`Storage::probe_objects`).
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// Neither the HEAD nor the PUT of the marker succeeded. The text names no key and no host.
    #[error("the object store is unreachable")]
    Unreachable,
}
```

Add `ProbeError` to the `pub use crate::error::{…}` list near line 78 of `lib.rs`.

In `objects.rs`, add `pub(crate) async fn probe(&self) -> Result<(), ProbeError>`:

1. `self.raw_store().head(&ObjectPath::from(PROBE_KEY))`, with `const PROBE_KEY: &str = "_probe/readyz";`. `raw_store()` is behind the storage prefix and skips the read cache. `Ok(_)` → `Ok(())`.
2. On **any** error, `self.raw_store().put(&ObjectPath::from(PROBE_KEY), PutPayload::from_static(b""))`. This covers both a missing marker (404) and S3's 403 for a missing key when the credentials lack `s3:ListBucket`. `Ok(_)` → `Ok(())`.
3. Otherwise `Err(ProbeError::Unreachable)`. Log once at `debug` with the two errors' kinds only, never their `Display`, which contains the URL.

**Behaviour:**
- The marker is written at most once per store in practice: once it exists, every probe is one HEAD.
- `_probe/` is outside `tables/` and `media/`, so GC, `collect_orphans` and backups never see it.
- In S3 mode `object_store` retries inside each call (its `RetryConfig`). The caller bounds the whole probe with a timeout.

In `lib.rs`:

```rust
/// HEADs the marker `_probe/readyz` and, if that fails, writes it (D-N-36): the store
/// answered and accepted the credentials. Needs no `s3:ListBucket`. No timeout here; the
/// caller sets one.
pub async fn probe_objects(&self) -> Result<(), ProbeError> {
    self.objects.probe().await
}
```

The probe writes even when `delete_objects` is `false`: it is an addition, not a deletion.

### 3. `Storage::stats`

In `lib.rs`:

```rust
/// Monotonic counters since open, and the WAL size now (`/metrics`, D-N-31).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageStats {
    /// Bytes of WAL segment files on disk: spans, then scores.
    pub wal_bytes: [u64; 2],
    /// Flushes committed, per table (spans, scores).
    pub flushes_ok: [u64; 2],
    /// Flush attempts that failed, per table.
    pub flushes_failed: [u64; 2],
    /// Wall time of committed flushes, nanoseconds, per table.
    pub flush_nanos: [u64; 2],
    /// Compaction jobs committed.
    pub compactions_ok: u64,
    /// Compaction jobs that failed.
    pub compactions_failed: u64,
    /// Compaction jobs deferred for lack of free disk.
    pub compactions_deferred: u64,
    /// Compaction jobs that lost a commit conflict (a later pass redoes them).
    pub compactions_conflict: u64,
    /// Bytes of Parquet objects written by compaction jobs.
    pub compaction_bytes_written: u64,
    /// Obsolete data files and staged objects deleted by GC. Media blobs are not counted.
    pub gc_objects_deleted: u64,
}
```

Re-export `StorageStats` next to `StorageHealth`.

**`Counters`** goes in the new `crates/storage/src/counters.rs`:
- one `AtomicU64` per scalar and `[AtomicU64; 2]` per per-table field (index 0 = spans, 1 = scores, from the existing `Table` enum);
- methods with `Ordering::Relaxed`: `flush_ok(table, nanos)`, `flush_failed(table)`, `compaction(outcome)`, `compaction_bytes(n)`, `gc_deleted(n)`, and `snapshot(&self) -> StorageStats` (with `wal_bytes` zero).

**The carrier is the `Committer`.**
- `Committer::open` (`catalog/committer.rs`) creates an `Arc<Counters>`, and `pub(crate) fn counters(&self) -> &Counters` returns it.
- Flush (`FlushParams`), compaction (`Compactor`, whose `committer` field is `pub(super)`) and GC (`collect_once`, `collect_obsolete` take `&Committer`) already reach it, so no signature changes.
- One concrete type, no trait.

**Counter sites.** Add each increment next to the existing code; do not restructure or rename anything.

| Field | Where |
|---|---|
| `flushes_ok`, `flush_nanos` | `flush.rs` `run()`. Take an `Instant` before `flush_once(&params).await`; in the `Ok(Some(seq))` arm, add one flush and the elapsed nanoseconds. `Ok(None)` (nothing to flush) counts nothing. |
| `flushes_failed` | `flush.rs` `run()`, the `Err(_)` arm of `flush_once`. Not in `log_retry`, which cannot reach the committer. |
| `compactions_ok`, `compaction_bytes_written`, `compactions_conflict`, `compactions_deferred`, `compactions_failed` | **in `Compactor::run`** (`compaction/job.rs`, near line 301), on its result just before it returns. Keep the function's name. If it has several `return`s, bind the result of its body to a local and count once at the end; do not add a wrapper function. The mapping: `Ok(report)` → `ok += report.jobs`, `bytes += report.output_bytes`; `Err(CompactionError::Commit(CommitError::Conflict { .. }))` → conflict; `Err(CompactionError::DiskFull)` (raised near line 330) → deferred; any other `Err` → failed. `run` is called by the pass and by `rewrite.rs`, so this covers both. Check that `rewrite.rs`'s own "not enough free disk" path (near line 399) defers through `run`; if it defers **before** calling `run`, count `compactions_deferred` there too. |
| `compactions_failed` (panics) | the `Err(_)` of `set.join_next()` in the pass loop: a panicked job never returned from `run`. |
| `gc_objects_deleted` | `gc.rs`: add `deleted` where `collected staged objects` (near line 174) and the obsolete-file summary (near line 254) are logged. |

Do not count from `CompactionReport.failed`: it lumps conflicts and deferrals with failures.

**`wal_bytes`.**
- `pub async fn stats(&self) -> StorageStats` takes `self.committer.counters().snapshot()`, then sums the sizes of the regular files in `data_dir/wal/spans` and `data_dir/wal/scores`. Use the same path construction the WAL writers use in `Storage::open`.
- Do it in one `spawn_blocking`, with `crate::fs::read_dir` and `crate::fs::metadata` (never `std::fs` directly).
- A `read_dir` or `metadata` error counts that file or directory as 0 and logs at `debug` with no path.

### 4. The deletion switch (D-N-34)

**`StorageConfig`** (`config.rs`) is `deny_unknown_fields`. The new field is skipped by serde, so it is **not** a TOML key, and `delete_objects = false` in a TOML file is rejected like any unknown key:

```rust
/// Restore mode (N M3, D-N-34): `false` makes every object deletion fail with
/// `ObjectError::DeletionOff`. Not a TOML key; the server sets it.
#[serde(skip, default = "default_true")]
pub delete_objects: bool,
```

Use an existing `default_true` helper if there is one; otherwise add it. Set `delete_objects: true` in every literal (see Edit only).

**`Objects`.**
- It gets a `delete_objects: bool` field, set from `config.delete_objects` in `Objects::new` and in `remote_for_test` / any other constructor (true by default for test constructors).
- `delete` and `delete_len` return `Err(ObjectError::DeletionOff)` as their first statement when it is `false`, before `refuse_after_sync_failure`.
- `ObjectError::DeletionOff`:
  - `#[error("object deletion is off (restore mode)")]`;
  - its `label()` is `"deletion_off"`, if `ObjectError` has `label`;
  - an arm in every exhaustive `match` on `ObjectError`.

Every caller already treats a delete error as "try again later" and keeps its catalog row, so nothing is lost: a later server without restore mode collects normally. With deletion off the GC task logs a warning on each tick that has something to collect. That noise is acceptable in a drill.

**`MediaGc::finish_drop`** (`media.rs`) must not reach `self.committer.finish_drop(project)` when a blob deletion fails. As written, `self.objects.delete(&path).await?` already returns early, so the drop stays `dropping` and is retried by a later server. Keep it that way, and add a comment at the `?`: "With deletion off (D-N-34) this returns before `finish_drop`, so the drop stays `dropping` and its blobs are deleted by a later server."

**`Maintenance::open`** (`maintenance.rs`) always builds `Objects` with deletion **on**, whatever `config.delete_objects` says. `gc-orphans` must delete even when the drill's config carries `restore_mode` (Q's drill runs it with the same `cetra.toml`). Clone the config and set `delete_objects = true` before `Objects::new`, with a comment saying why.

**Log** once at `Storage::open`, when off: `tracing::warn!("object deletion is off (restore mode)")`.

**Nothing else changes.** These paths do not delete objects through `Objects`, so leave them:
- compaction, retention and deletion (they obsolete files in the catalog);
- staged rows;
- the read cache;
- spill;
- snapshot staging;
- temp-file cleanup.

## Spec

Two edits, one per section:

1. **§ 5 (GC), at the end, add:** "A server in restore mode opens the storage with object deletion off (`StorageConfig::delete_objects`, N M3, D-N-34): every object deletion fails and is retried by a later server, so GC, media collection and project drops remove nothing. `gc-orphans` still deletes."
2. **§ 10, after the free-disk paragraph, add:**

> The server reads free disk (`StorageHealth::disk_low`, `disk_free_bytes`) for `/readyz` and `/metrics`. It probes the object store by HEADing the marker `_probe/readyz` and, if that fails, writing it (`Storage::probe_objects`, D-N-36; no `s3:ListBucket` needed). It reads monotonic counters (`Storage::stats`) for `/metrics` (N M3, D-N-30, D-N-31).

## Tests (in the storage crate's existing test modules)

**Free disk:**
- `health_reports_low_disk`: open a storage, then `storage.disk().set_free_for_test(Some(0))`. Now `health().disk_low` is `true` and `disk_free_bytes == 0`. Set `Some(u64::MAX)` and it is `false`. `disk_min_free_bytes` equals the config's `min_free_bytes`.

**The probe:**
- `probe_objects_writes_the_marker_once`: on a fresh local-mode storage, `probe_objects()` is `Ok(())` and `_probe/readyz` now exists in the store. A second call is `Ok(())`.
- `probe_objects_fails_when_the_store_fails`: with `ChaosStore` (`test_support/chaos.rs`) scripted to fail every `Head` and `Put` with a non-`NotFound` error, the result is `Err(ProbeError::Unreachable)`.
- `probe_objects_writes_when_head_is_refused`: with `ChaosStore` failing `Head` only, the result is `Ok(())`. This is the 403-without-ListBucket case.

**Counters:**
- `stats_count_a_flush`: append spans, call `flush_now`, then `stats().flushes_ok[0] >= 1`, `flush_nanos[0] > 0` and `flushes_failed[0] == 0`.
- `stats_report_wal_bytes`: after an append and before a flush, `stats().wal_bytes[0] > 0`.
- `stats_count_gc_deletions`: in an existing GC test that deletes obsolete files, `gc_objects_deleted` grows by the number deleted.
- `stats_count_a_compaction`: in an existing compaction test that commits a job, `compactions_ok >= 1` and `compaction_bytes_written > 0`. In an existing test where a job loses a conflict, `compactions_conflict == 1` and `compactions_failed == 0`.

**The deletion switch:**
- `no_object_is_deleted_when_delete_objects_is_off`: open with `delete_objects: false` and `gc.grace` at its minimum. Obsolete a file as the existing GC tests do, then wait past the grace and run the collection. The object is still in the store, and its catalog row is still obsolete (not removed).
- `drop_stays_dropping_when_delete_objects_is_off`: as the existing drop tests do, store a media blob for a project and drop it with `delete_objects: false`. The blob is still in the store, and the project is still listed by `committer.dropping_projects()`.
- `maintenance_deletes_even_when_delete_objects_is_off`: the existing `collect_orphans` test (`maintenance.rs`) run with `delete_objects: false` in the config still deletes the orphan.
- `delete_objects_is_not_a_toml_key`: a `StorageConfig` deserialized from TOML has `delete_objects == true`, and a TOML with `delete_objects = false` is rejected (`deny_unknown_fields`).

## Verify

```bash
cargo test -p cetra-storage --all-features low_disk
cargo test -p cetra-storage --all-features probe_objects
cargo test -p cetra-storage --all-features stats_
cargo test -p cetra-storage --all-features delete_objects
cargo test -p cetra-storage --all-features maintenance_deletes
just check
```

## Done when

- [ ] The three APIs and the deletion switch exist with doc comments. `StorageHealth`, `StorageStats` and `ProbeError` are `pub` and re-exported.
- [ ] The twelve tests pass, and spec 03 § 5 and § 10 have their text.
- [ ] `just check` passes. One commit.
