# Storage hardening: findings

Track R (`docs/plans/r-hardening/`) checks the storage engine against its spec and records what it finds here. Each milestone has a section. Each finding has an id, `M<milestone>-<n>`, which the commits and the plan refer to.

## M1: Model-based correctness

The model test in `crates/storage/src/model_tests/` (plan `docs/plans/r-hardening/01-m1-model.md`, decisions D-R-3 and D-R-4). A proptest runs random operations against the real `Storage` and against a small in-memory model. After every operation it reads storage back through the same read functions the server calls and compares the results with the model.

What it covers:
- **Writes:** span appends and re-sends, score versions and score deletions, across two projects that share trace numbers.
- **Reads:** the span list (paged), the trace list (windowed, with and without evaluation traces), the score list, scores by id and latest scores, sessions, users, metrics KPIs, the filter-deletion trace ids and alert measures.
- **Deletions:** trace and user deletions, their scope reads, evaluation traces, the applied status, retention and project drop.
- **Lifecycle:** restarts and in-process crashes (the memtables are lost, the WAL is kept; lossy crashes come in M2), compaction, garbage collection and settling.
- **Settled files:** after each settle, the files on disk hold no row a deletion covers, nothing before the retention watermark, no file of a dropped project, no duplicate span, no unmerged score, one partial per trace (two for class-2 traces that cross midnight), and no staged object.

### Runs

| Date | Commit | Cases | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | R1.7 | 500 | release | not run | deferred (long run) |

### Findings

| Id | Shrunk case | Cause | Classification | Fix |
|---|---|---|---|---|
| M1-1 | `regression_m1_1_span_paging_with_duplicates` | `span_list` returned duplicate copies of a re-sent span, and its keyset cursor could not tell them apart, so pages depended on the page size. | storage | e002170 and 8e907d8: sort `ingest_time DESC` last, drop adjacent copies per page, bound the refetch |
| M1-2 | `stale_score_after_day_close_of_tombstone` | The day close dropped a score's tombstone with the older versions it shadowed. A version with an older `updated_at` sent later then had nothing to lose to, and read as live. The model kept the tombstone, so it expected the score gone. | storage, documented known limit | Spec 03 § 5 states the known limit (the tombstone is the score's own; the bound is the writer and `close_delay`), and the settle step mirrors the day close in the model (`drop_day_closed_scores`). This commit |

## M2: Durability

- **M2-R2.8 (unverified):** a `catalog.db` commit that fails with `SQLITE_IOERR_FSYNC` or `SQLITE_IOERR_DIR_FSYNC` stops the committer (D-R-6). The reason is that in WAL mode the commit's frames may already be in `catalog.db-wal`, and SQLite's WAL recovery at the next open may apply them. That reading of SQLite's `wal.c` is not tested. LazyFS cannot inject fsync errors, so no test covers it either. Record it as unverified until a fault-injecting filesystem or a SQLite-level test covers it.
- **M2-R2.8 (unverified): an in-transaction I/O error only rolls back.** A statement inside a transaction that fails with `SQLITE_IOERR_WRITE` or `SQLITE_IOERR_READ` is not a commit point: its error is rolled back and stays retryable, and the committer keeps running. The reason is that SQLite's page cache can spill WAL frames to `catalog.db-wal` mid-transaction, and an uncommitted transaction's frames are not applied at the next open. The reading is not tested, since no fault-injecting filesystem reaches SQLite's WAL writes.
- **M2-R2.8 (unverified): a failed autocheckpoint fsync is invisible to the committer.** SQLite ignores an fsync error during an autocheckpoint that follows a successful `COMMIT`. That `commit()` returns `Ok`, so `stop_on_fatal` never sees the error and the committer keeps running. Recorded so that a later finding of this kind is not read as a gap in `stop_on_fatal`. Not tested.
- **M2-R2.8 (unverified): `/readyz` reports a stop only once the server request lands.** Spec 03 §10 requires a stopped component to show as `/readyz` 503. That holds only after the `crates/server` change of R2.7 and R2.8 (`health.rs` reads `objects_sync_failed` and `catalog_failed`) lands. Until then a running binary reports ready after a stop.
