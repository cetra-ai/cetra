# 01: free disk, the object-store probe and counters from storage

Read [00-design.md](00-design.md) first, especially § Storage API needed from track R (D-N-31).

> **Gate.** `crates/storage` is track R's area. Do not start this phase until the coordinator says that R approved it, or that R implements it. If you are R, the API below is the contract: the names, types and meaning must match, because phases 02 and 07 use them.

**Goal:** add three read-only views to `llmobs_storage::Storage` for the server's `/readyz` and `/metrics`, plus one switch for restore mode (D-N-34). With the switch at its default, nothing changes behaviour.
- `StorageHealth` gets the free-disk fields.
- `Storage::probe_objects()` does one `HEAD` against the object store.
- `Storage::stats()` returns monotonic counters plus the WAL size.
- `StorageConfig::delete_objects` (default `true`) turns object deletion off when `false`.

**Edit only:**
- `crates/storage/src/lib.rs`;
- `crates/storage/src/disk.rs`;
- `crates/storage/src/objects.rs`;
- `crates/storage/src/error.rs`;
- the counter sites in `crates/storage/src/flush.rs`, `crates/storage/src/compaction/job.rs` and `crates/storage/src/gc.rs`;
- `crates/storage/src/catalog/committer.rs` (it carries the counters);
- a new `crates/storage/src/counters.rs`;
- `crates/storage/src/config.rs` (`delete_objects`) and every `StorageConfig { … }` literal, including three in `crates/server` (§ 4 lists them);
- `crates/storage/src/media.rs`, if the media GC deletes outside `gc.rs`;
- `docs/spec/03-storage.md` (§5 and §10, one paragraph each).

**Commit:** `Expose disk, object-store probe and counters from storage`

**Needs:** R's OK (gate above).

**What not to change:** the `Maintenance` open (`maintenance.rs`) and `gc-orphans` keep deleting as today; `delete_objects` concerns `Storage::open` only.

## Read first

- `crates/storage/src/lib.rs`: `StorageHealth`, `Storage::health`, the `disk`, `objects`, `spans` and `scores` fields, `Storage::read_cache_stats`.
- `crates/storage/src/disk.rs`: `Disk::free`, `Disk::low`, the `min_free` field.
- `crates/storage/src/objects.rs`: `Objects::store()` and `raw_store()`, and how `head` is called near line 388.
- `crates/storage/src/flush.rs`: where a flush commit succeeds and where a flush attempt fails.
- `crates/storage/src/compaction/job.rs` lines 170–280: the job outcomes (done, failed, "deferred: not enough free disk").
- `crates/storage/src/gc.rs`: `collect_obsolete` and `collect_once`, which count `deleted`.

## Changes

### 1. `StorageHealth`, free disk

Add three fields to `StorageHealth`, with doc comments:

```rust
/// Free disk is under `storage.min_free_bytes`: ingestion is refused (spec 03 §10).
pub disk_low: bool,
/// The least free space of the data directory and the local object root, from the last
/// measurement (every second).
pub disk_free_bytes: u64,
/// `storage.min_free_bytes`.
pub disk_min_free_bytes: u64,
```

In `disk.rs`, make `min_free` readable with `pub(crate) fn min_free(&self) -> u64`. `Storage::health` fills the fields from `self.disk.low()`, `self.disk.free()` and `self.disk.min_free()`. Update every `StorageHealth { … }` literal in tests, if any: `grep -rn "StorageHealth {" crates/`.

### 2. `Storage::probe_objects`

In `error.rs`:

```rust
/// The object store did not answer a probe (`Storage::probe_objects`).
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The store failed or timed out. The text names no key and no host.
    #[error("the object store is unreachable")]
    Unreachable,
}
```

Re-export it like the other errors (`pub use error::ProbeError` if `lib.rs` re-exports errors that way; otherwise as `llmobs_storage::error::ProbeError`).

In `objects.rs`, add `pub(crate) async fn probe(&self) -> Result<(), ProbeError>`:
- It runs `self.raw_store().head(&ObjectPath::from("_probe/readyz"))`. `raw_store()` is already behind the storage prefix (`PrefixStore`), and it skips the read cache (`store()` is the `CacheStore`).
- `Ok(_)` and `Err(object_store::Error::NotFound { .. })` both give `Ok(())`. Any other error gives `Err(ProbeError::Unreachable)`, and the error is logged once at `debug` with the error's kind only. Do not log its `Display`: it contains the URL.
- No retry of its own. In S3 mode `object_store` retries inside the call (its `RetryConfig`); the server bounds the probe with a 2 s timeout. Say so in the doc comment.

In `lib.rs`:

```rust
/// One `HEAD` of a fixed absent key. `NotFound` counts as reachable: the store answered and
/// accepted the credentials. No timeout here; the caller sets one.
pub async fn probe_objects(&self) -> Result<(), ProbeError> {
    self.objects.probe().await
}
```

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
    /// Objects deleted by GC (obsolete data files and staged objects).
    pub gc_objects_deleted: u64,
}
```

Add `pub(crate) struct Counters` in the new `crates/storage/src/counters.rs`:
- one `AtomicU64` per scalar and `[AtomicU64; 2]` per per-table field (index 0 = spans, 1 = scores; use the existing `Table` enum to index);
- small methods with `Ordering::Relaxed`: `flush_ok(table, nanos)`, `flush_failed(table)`, `compaction(outcome)`, `compaction_bytes(n)`, `gc_deleted(n)`, and `snapshot(&self) -> StorageStats` (without `wal_bytes`).

**The carrier is the `Committer`** (`catalog/committer.rs`):
- `Committer::open` creates `Arc<Counters>`;
- `pub(crate) fn counters(&self) -> &Counters` returns it.

Flush (`FlushParams`), compaction (the `Compactor`) and GC (`GcStart`, `collect_once`, `collect_obsolete`) already hold an `Arc<Committer>`, so no function signature changes. If one of them reaches the committer only through another field, follow that field; do not add parameters. One concrete type, no trait.

**Counter sites.** Add the increment next to the existing code; do not restructure it. (`Storage::open` builds the committer with `Committer::open`, `catalog/committer.rs`.)

| Field | Where |
|---|---|
| `flushes_ok`, `flush_nanos` | `flush.rs` `run()`, the `Ok(Some(seq))` arm: a flush committed. Take an `Instant` before `flush_once(&params).await` and add its elapsed nanoseconds. (`Ok(None)` means nothing to flush: count nothing.) |
| `flushes_failed` | `flush.rs` `run()`, the `Err(_)` arm of `flush_once` (near line 197). `log_retry` has no access to the committer; do not count there. |
| `compactions_ok`, `compaction_bytes_written`, `compactions_conflict`, `compactions_deferred`, `compactions_failed` | **inside `Compactor::run`** (`compaction/job.rs`), on its result before it returns: `Ok(report)` → `ok += report.jobs`, `bytes += report.output_bytes`; `Err(CompactionError::Commit(CommitError::Conflict { .. }))` → conflict; `Err(CompactionError::DiskFull)` → deferred; any other `Err` → failed. `run` is called both by the pass closure and by `rewrite.rs`, so counting there covers both once. Wrap the body (e.g. rename it `run_inner`) rather than adding a counter at each `return`. |
| `compactions_failed` (panics) | the `Err(_)` of `set.join_next()` in the pass's aggregation loop: a panicked job never returned from `run` |
| `gc_objects_deleted` | `gc.rs`: add `deleted` where `collected staged objects` (near line 174) is logged and where the obsolete-file summary (near line 254) is logged |

Do not count from `CompactionReport.failed`: it lumps conflicts and deferrals with failures. Check that `rewrite.rs`'s own "not enough free disk" path goes through `Compactor::run`'s `DiskFull`; if it defers before calling `run`, count `compactions_deferred` there too.

**`wal_bytes`** is not a counter.
- `pub async fn stats(&self) -> StorageStats` reads the counters, then sums the sizes of the regular files in each table's WAL directory in one `spawn_blocking` (`read_dir`, then `metadata().len()`).
- Use the same directory paths the writers use; find them where `Storage::open` builds the WAL writers.
- A `read_dir` or `metadata` error counts that file or directory as 0 and logs at `debug` with no path.

### 4. `StorageConfig::delete_objects` (D-N-34)

In `config.rs`, `StorageConfig` gets:

```rust
/// Restore mode (N M3, D-N-34): `false` deletes no object. The GC task is not started,
/// and `collect_obsolete` and the media GC delete nothing. Not read from TOML; the server
/// sets it.
#[serde(skip, default = "default_true")]
pub delete_objects: bool,
```

- Use the existing `default_true` helper if there is one; otherwise add it.
- Set `delete_objects: true` in **every** `StorageConfig { … }` literal, or the workspace does not compile. `grep -rn "StorageConfig {" crates/` finds them:
  - in `crates/storage`: `src/` (`objects.rs`, `memory.rs`, `s3_fault_tests.rs`, `query/{spans,scores,traces}.rs`, `model_tests/harness.rs`, `model_tests/resources/memory.rs`, the crash, deletion and drop tests, …);
  - `tests/{crash,ingest_bench,s3,s3_chaos,trace_bench}.rs`;
  - **three in `crates/server`**: `src/ingest/fixtures_tests.rs`, and `src/test_support/mod.rs` (two). This phase may edit those three literals, and only to add the field.

**The delete sites.** Every object deletion in `Storage` goes through four functions. Each one returns early, deleting nothing and changing no catalog row, when deletion is off:

| Function | Called from |
|---|---|
| `gc::collect_once` (`gc.rs`, staged objects) | open (`lib.rs`, before the obsolete-file block), the GC task, `Storage::collect_staged` |
| `gc::collect_obsolete` (`gc.rs`, obsolete data files) | open, the GC task, `Storage::collect_obsolete` |
| `MediaGc::collect` (`media.rs`, stale media) | the compaction task (`compaction/task.rs`), the GC task |
| `MediaGc::finish_drop` (`media.rs`, a dropped project's media) | `Storage::drop_project`, `media.finish_drops()` at open, the GC task |

Give each one access to the flag:
- `MediaGc` gets a `delete_objects: bool` field, set from the config where `Storage::open` builds it;
- `gc::collect_once` and `gc::collect_obsolete` get it through what they already receive. If neither the `Committer` nor `Objects` is a natural home, add a `delete_objects: bool` parameter: these are `pub(crate)` functions with few callers. Update the callers and their tests.

An early return must leave the catalog unchanged. The rows stay, so a later server without restore mode collects them normally. For `finish_drop`, the drop's catalog side still commits; only the media blob deletion and its row removal are skipped. Read `finish_drop` and keep the part that is not a deletion.

In `Storage::open`, when deletion is off:
- also do not call `gc::spawn`. Make the `gc` field an `Option<GcHandle>`; `self.gc.abort()` is called in two places (`Storage::shutdown` and `Drop`), and both must handle `None`.
- Log once: `tracing::warn!("object deletion is off (restore mode)")`.

Nothing else in `crates/storage` deletes objects. Flush and compaction failures leave staged rows for GC. `maintenance.rs` (`gc-orphans`) is a separate open and keeps deleting.

## Spec

In `docs/spec/03-storage.md` §10, after the free-disk paragraph, add:

Two edits, one per section:

1. In § 5 (GC), at the end, add the sentence: "A server in restore mode opens the storage with object deletion off (`StorageConfig::delete_objects`, N M3, D-N-34): GC, media collection and project drops delete no object."
2. In § 10, after the free-disk paragraph, add the paragraph:

> The server reads free disk (`StorageHealth::disk_low`, `disk_free_bytes`) for `/readyz` and `/metrics`. It also probes the object store with one `HEAD` of `_probe/readyz` (`NotFound` = reachable) and reads monotonic counters (`Storage::stats`) for `/metrics` (N M3, D-N-30, D-N-31).

## Tests (in the storage crate's existing test modules)

- `health_reports_low_disk`: open with `min_free_bytes` far above the real free space (e.g. `ByteSize::pib(1)`) and wait for one measurement, as the existing disk tests do with their `statvfs` stand-in. Then `health().disk_low` is `true` and `disk_min_free_bytes` equals the setting. With `ByteSize::b(0)` it is `false`.
- `probe_objects_succeeds_on_a_local_store`: a fresh local-mode storage gives `Ok(())`, and no `_probe` object exists afterwards.
- `probe_objects_fails_when_the_store_fails`: with the fault store (`ChaosStore` in `objects.rs` tests, or `test_support`) failing every `head` with a non-`NotFound` error, the probe gives `Err(ProbeError::Unreachable)`.
- `stats_count_a_flush`: append spans, call `flush_now`, then `stats().flushes_ok[0] >= 1`, `flush_nanos[0] > 0` and `flushes_failed[0] == 0`.
- `stats_report_wal_bytes`: after an append and before a flush, `stats().wal_bytes[0] > 0`.
- `stats_count_gc_deletions`: in an existing GC test that deletes obsolete files, `gc_objects_deleted` grows by the number deleted.
- `stats_count_a_compaction`: in an existing compaction test that commits a job, `compactions_ok >= 1` and `compaction_bytes_written > 0`. In an existing test where a job loses a conflict, `compactions_conflict == 1` and `compactions_failed == 0`.
- `staged_object_survives_reopen_when_delete_objects_is_off`: create an old staged object as the existing staged-GC tests do, reopen with `delete_objects: false` (and back-date it past 1 h the way those tests do), and it is still in the store and in the staged table.
- `drop_project_keeps_media_when_delete_objects_is_off`: as the existing drop tests do, store a media blob for a project and drop the project with `delete_objects: false`; the blob object is still in the store.
- `no_object_is_deleted_when_delete_objects_is_off`: open with `delete_objects: false` and `gc.grace` at its minimum. Obsolete a file through compaction or retention, as the existing GC tests do. Wait past the grace and call `collect_obsolete`: it reports 0, and the object is still in the store.
- `delete_objects_defaults_to_true`: a `StorageConfig` deserialized from TOML has `delete_objects == true`, and `delete_objects = false` in the TOML is ignored (`serde(skip)`) or rejected by `deny_unknown_fields`. Assert whichever the config does today.

## Verify

```bash
cargo test -p llmobs-storage --all-features low_disk
cargo test -p llmobs-storage --all-features probe_objects
cargo test -p llmobs-storage --all-features stats_
cargo test -p llmobs-storage --all-features delete_objects
just check
```

## Done when

- [ ] The three APIs and `delete_objects` exist with doc comments. `StorageHealth` and `StorageStats` are `pub` and re-exported where `StorageHealth` is.
- [ ] The eleven tests pass, and spec 03 §5 and §10 have their sentences.
- [ ] `just check` passes. One commit.
