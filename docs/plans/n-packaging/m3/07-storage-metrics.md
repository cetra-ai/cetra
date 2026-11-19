# 07: export the storage counters and disk gauges

Read [00-design.md](00-design.md) first, especially § `/metrics` and § Storage API needed from track R.

**Goal:** `/metrics` shows phase 01's free-disk fields and `StorageStats`:
- `cetra_storage_disk_free_bytes`, `cetra_storage_disk_min_free_bytes`;
- `cetra_storage_wal_bytes{table}`;
- `cetra_storage_flushes_total{table,outcome}`, `cetra_storage_flush_seconds_total{table}`;
- `cetra_storage_compactions_total{outcome}` (`ok`, `failed`, `deferred`, `conflict`), `cetra_storage_compaction_bytes_written_total`;
- `cetra_storage_gc_objects_deleted_total`. Its HELP says "obsolete data files and staged objects (including staged media uploads); media blobs collected by media GC are not counted".

**Edit only:** `crates/server/src/telemetry.rs`.

**Commit:** `Export the storage counters as metrics`

**Needs:** 01 and 05.

## Changes

1. **Register the metrics** in `Telemetry::new()` on the `cetra` sub-registry, counters **without** `_total` (e.g. `storage_flushes`), with the label enums `Table` (`spans`, `scores`) and the outcomes of the 00-design table. `Table` may already exist from phase 05; reuse it.
2. **Refresh them in `render`,** after the existing refreshes: one `state.storage.stats().await` and the `health()` already read.
   - The gauges are set directly.
   - `StorageStats` holds totals since open, but a prometheus-client `Counter` only supports `inc_by`. Keep the last seen value of each total in the same "last seen" store phase 05 uses for the read cache, and `inc_by(new.saturating_sub(last))`. Write one helper, `fn advance(counter: &Counter, last: &AtomicU64, now: u64)`, used for every total, including the read cache's (refactor phase 05's two call sites onto it).
   - `storage_flush_seconds` (rendered `cetra_storage_flush_seconds_total`) is a `Family<TableLabel, Counter<f64, AtomicU64>>`, advanced by `(now_nanos - last_nanos) as f64 / 1e9`. Give it its own float version of `advance`, or make `advance` take the delta and the counter separately; keep one helper per counter type.
3. **No storage call happens outside `render`.** The counters only change when Prometheus scrapes, which is fine for counters.

## Tests (`telemetry.rs`)

- `storage_counters_follow_a_flush`:
  - On a `TestApp`, ingest one span (or append through `state.storage`), call `state.storage.flush_now(…)` as the backup tests do, then `render`.
  - Then `cetra_storage_flushes_total{table="spans",outcome="ok"}` ≥ 1, and `cetra_storage_flush_seconds_total{table="spans"}` > 0.
  - A second `render` with no new flush shows the same values: the delta logic does not double-count.
- `disk_gauges_are_exported`: `cetra_storage_disk_free_bytes` > 0, and `cetra_storage_disk_min_free_bytes` equals the TestApp's `storage.min_free_bytes` in bytes.
- `wal_bytes_are_exported`: after an append and before a flush, `cetra_storage_wal_bytes{table="spans"}` > 0.
- `advance_never_goes_backwards`: a unit test of `advance`. Totals 5, then 9, then 7 (a value going down is impossible, but must not panic or wrap) give a counter of 9.

## Verify

```bash
cargo test -p cetra-server --all-features telemetry storage_counters
just check
```

## Done when

- [ ] The storage metrics of the 00-design table render, and they are monotonic across scrapes.
- [ ] The tests pass.
- [ ] `just check` passes. One commit.
