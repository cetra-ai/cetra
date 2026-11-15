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

The durability milestone (plan `docs/plans/r-hardening/02-m2-durability.md`, decisions D-R-5, D-R-6 and D-R-13). Every local file call of `crates/storage` goes through the seam `src/fs.rs`, statvfs included. Lossy mode (`src/fs/lossy.rs`, test-only) records each registered file's synced and unsynced writes and, at a crash, applies one of three modes chosen by a seed: `DropAll`, `KeepRandom` and `TearLast`. Lossy crashes run in the model test (R2.9), in the hook-abort children (R2.10), in the model's hook crash (R2.11) and in unit tests of each write path. The LazyFS cross-check (`tools/lazyfs/`, R2.12) runs the crash children on the real binary. It is pending (see § LazyFS and the shim).

### Runs

| Date | Commit | Command | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | 7a7bae5 | `LLMOBS_HOOK_CRASH_CASES=52 cargo test -p llmobs-storage --all-features --lib -- --ignored --exact model_tests::hook_crash::every_model_hook_crash_reads_back` | debug, local | 102 s | pass: 52 cases, 44 aborted at their hook point, 8 completed without reaching it (cases 14, 18, 19, 41 to 45); 26 lossy cases among them |
| 2026-11-15 | 7a7bae5 | the same, default count (25) | debug, local | 52.8 s | pass: 25 cases, 22 aborted, 3 completed (14, 18, 19) |
| | | R2.9 long run, R2.10 with `LLMOBS_LOSSY_SEEDS=10`, R2.11 with `LLMOBS_HOOK_CRASH_CASES=500`, `just lazyfs` three times, the GitHub FUSE probe | | | pending (R2.14) |

### Findings

Each finding is confirmed by a test that failed before its fix (the test-first step of its commit). The assertion text is not recorded in the commits, so it is not copied here; R2.14 re-runs each test against its parent commit to record it.

| Id | Audit item | Test | Cause | Fix commit |
|---|---|---|---|---|
| M2-1 | 1: rotation of an interval-mode WAL leaves the old segment unsynced | `interval_rotation_syncs_the_old_segment` | A power loss could tear a non-last segment, and the next start refused it as corrupt | 7b21006 |
| M2-2 | 1b: open leaves the replayed segment unsynced once it is no longer the last | `reopen_syncs_the_replayed_segment` | The same tear, after a reopen | 7b21006 |
| M2-3 | 2: a deleted object can return after a power loss | `a_deleted_object_stays_deleted_after_a_lossy_crash`, `gc_leaves_no_orphan_after_a_lossy_crash` | The unlink was not followed by an fsync of its directory | b14ca21 (its relative-path regression: 6634c79) |
| M2-4 | 3: a failed WAL fsync is retried on the same file | `a_failed_sync_leaves_the_group_and_does_not_sync_again` | A second fsync after a failed one proves nothing, since the kernel may have dropped the dirty pages | 04876cf |
| M2-5 | 3b: a discarded segment comes back after a power loss | `a_discarded_segment_stays_removed_after_a_lossy_crash` | The removal of the new empty segment was not fsynced; it returned sorted after the active segment, and the next open failed | 04876cf |
| M2-6 | 4: a torn read-cache size file serves short ranges | `a_torn_size_file_never_serves_short_ranges` | The size was decimal text with no check, so a prefix of its digits parsed | 7977cd7 |
| M2-7 | 5: a catalog I/O error does not stop the committer | `an_fsync_error_stops_the_committer_until_reopen`, `fatal_io_codes_stop_the_committer_and_the_rest_stay_retryable` | The committer kept running after `SQLITE_IOERR_FSYNC` and the other fatal I/O codes | 47f5f4f (runtime and commit points: b504857) |
| M2-8 | 6: new directory entries are not fsynced | `an_object_in_a_directory_left_unsynced_survives_a_lossy_crash`, `an_outside_object_root_under_an_unsynced_ancestor_survives_a_lossy_crash`, `the_data_dir_layout_is_durable_after_a_crash_before_its_sync` | `create_dir_chain` made directories without syncing their entries, and an outside root's nearest existing ancestor was not synced before the walk | df7a009 (ancestor), b14ca21 (the chain) |

Other fixes in the milestone, not audit items:
- 02f2509 (R2.7): a failed object fsync stops the local object store (D-R-6). Tests `a_failed_object_fsync_refuses_later_writes_and_deletes`, `a_failed_delete_sync_stops_the_store`.
- b504857 (R2.7, R2.8): a runtime directory sync error was mapped to a plain I/O error, so `sync_failed` stayed false; every catalog commit point now stops the committer on an fsync error. Tests `a_failed_sync_of_a_new_day_directory_stops_the_store`, `every_commit_point_stops_the_committer_on_an_fsync_error`.
- 6634c79: a regression that b14ca21 introduced. The empty parent of a one-component relative path made every start fail once the open fsynced parents. Test `a_relative_one_component_path_syncs_the_current_directory`.

### LazyFS and the shim

Pending (external). The LazyFS cross-check (R2.12) needs FUSE and a real run of `just lazyfs`, so none of it ran in this milestone's local work. Until it does, these are open:
- what LazyFS loses at each crash, compared with the shim's three modes;
- which findings M2-1 to M2-8 LazyFS reproduces, and the pre-fix check of R2.12 step 3 for each;
- any shim bug LazyFS shows. None are known.

### Unverified

- **M2-R2.8 (unverified):** a `catalog.db` commit that fails with `SQLITE_IOERR_FSYNC` or `SQLITE_IOERR_DIR_FSYNC` stops the committer (D-R-6). The reason is that in WAL mode the commit's frames may already be in `catalog.db-wal`, and SQLite's WAL recovery at the next open may apply them. That reading of SQLite's `wal.c` is not tested. LazyFS cannot inject fsync errors, so no test covers it either. Record it as unverified until a fault-injecting filesystem or a SQLite-level test covers it.
- **M2-R2.8 (unverified): an in-transaction I/O error only rolls back.** A statement inside a transaction that fails with `SQLITE_IOERR_WRITE` or `SQLITE_IOERR_READ` is not a commit point: its error is rolled back and stays retryable, and the committer keeps running. The reason is that SQLite's page cache can spill WAL frames to `catalog.db-wal` mid-transaction, and an uncommitted transaction's frames are not applied at the next open. The reading is not tested, since no fault-injecting filesystem reaches SQLite's WAL writes.
- **M2-R2.8 (unverified): a failed autocheckpoint fsync is invisible to the committer.** SQLite ignores an fsync error during an autocheckpoint that follows a successful `COMMIT`. That `commit()` returns `Ok`, so `stop_on_fatal` never sees the error and the committer keeps running. Recorded so that a later finding of this kind is not read as a gap in `stop_on_fatal`. Not tested.
- **M2-R2.8 (unverified): `/readyz` reports a stop only once the server request lands.** Spec 03 §10 requires a stopped component to show as `/readyz` 503. That holds only after the `crates/server` change of R2.7 and R2.8 (`health.rs` reads `objects_sync_failed` and `catalog_failed`) lands. Until then a running binary reports ready after a stop.
- **M2-R2.5 (unverified): a WAL truncation whose directory fsync fails.** The flush task retries it on the next flush (spec 03 § Durability, why this is safe). No test covers the retry.
- **M2 pre-fix assertion texts.** The findings above rest on the test-first runs recorded in their commits. The texts are not recorded, and R2.14 records them.

## M4 — S3 fault injection

The S3 fault milestone (plan `docs/plans/r-hardening/04-m4-s3-faults.md`, decision D-R-8). `ChaosStore` (`crates/storage/src/test_support/chaos.rs`) sits in front of an in-memory bucket and fails or delays the calls it forwards, as `object_store`'s S3 client does once its retries are exhausted. The model test has an S3 mode (`model_tests::storage_matches_the_model_on_s3`) that runs the same ops against it with the read cache on.

What the S3 mode covers:
- **Faults:** a seeded schedule (`Rates::MODEL`) injects delays and before and after failures on puts, multipart parts and completes, deletes, reads (errors and truncated bodies), and aborts. Faults are on during each op but `Settle`, and off during the checks and the final settle.
- **Failure rule:** an op may fail only if the chaos store injected a fault during it. The model never changes on a failure.
- **Calm checks:** every read that is compared with the model runs with the schedule off. `FaultyRead` is the one op that reads under faults.
- **S3-only ops:** `Verify` (a verify pass on an active file, with no quarantine or repair allowed), `EvictCache`, and `FaultyRead`.
- **Upload ledger:** every multipart upload is recorded. `leaked_uploads` must be empty at each check; `abandoned_uploads` (a failed abort, a lost create answer, a life cut short) is counted in the statistics only.
- **Audit and never listed:** the orphan audit and the read-cache-equals-store check of R4.4, and the rule that storage never lists or copies the bucket outside the audit.

### Runs

| Date | Commit | Command | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | R4.5 | `LLMOBS_MODEL_S3_CASES=8 cargo test -p llmobs-storage --all-features --lib model_tests::storage_matches_the_model_on_s3` | debug, in-memory bucket | 32.7 s | pass |
| 2026-11-15 | R4.5 | `LLMOBS_MODEL_S3_CASES=50 LLMOBS_MODEL_S3_STATS=1 cargo test -p llmobs-storage --all-features --lib model_tests::storage_matches_the_model_on_s3` | debug, in-memory bucket | 279.5 s | pass: 50 cases, 51 injected faults, 446 puts, 325 multipart completes, 29 aborts, 11 abandoned uploads, 0 `FaultyRead` errors |
| 2026-11-15 | R4.5 | `LLMOBS_MODEL_S3_CASES=4 cargo test -p llmobs-storage --all-features --lib model_tests::storage_matches_the_model_on_s3`, nine runs | debug, in-memory bucket | 5.7 s to 98 s (8 of 9 under 45 s) | pass; the default is now 4. At 8 cases five runs took 10 s to 103 s. A shutdown whose flush fails under faults waits out its 30 s grace period |
| | | `just model-s3 200` (the first row planned) | release | | deferred (long run) |

### Findings

None yet. The S3 mode of R4.5 found no disagreement in the 50-case run above.

## Gaps toward deterministic simulation

Counts from the plan's commands (D-R-13), run from the repository root. Each filter drops only lines that contain `test`, so some test code in production files is still counted; the counts are an upper bound for production code.

```bash
grep -rnE 'SystemTime::now|Instant::now|now_ns\(\)' crates/storage/src --include=*.rs | grep -v test   # 100
grep -rnE 'tokio::time::(sleep|interval|timeout|Instant)' crates/storage/src --include=*.rs | grep -v test # 74
grep -rn 'spawn_blocking' crates/storage/src --include=*.rs | grep -v test                               # 43
grep -rnE 'now_v7|new_v7' crates/storage/src --include=*.rs | grep -v test                               # 29
grep -rnE 'HashMap|HashSet' crates/storage/src --include=*.rs | grep -v test                             # 127
```

The gaps:
- **Wall-clock reads** (100 `SystemTime`, `Instant` and `now_ns` sites): the tokio timers of the writer's group wait and sync interval (74 timer sites in all), and the flush, GC, compaction and disk-monitor intervals.
- **Threads outside the async runtime** (43 `spawn_blocking` sites): WAL commits, object writes, Parquet encoding, replay and cleanup.
- **UUIDv7 randomness** (29 sites): file names, temp names and deletion ids.
- **Hash iteration order** (127 `HashMap` and `HashSet` sites, `RandomState`).
- **SQLite's own I/O**, which would need a VFS. LazyFS covers it for now (D-R-5).
- **DataFusion's reads and spill files**, outside the seam.
- **`object_store`'s HTTP client and its retries.**
- **Tokio's multi-thread scheduling.**

What M2 closed: every local file call of the crate, statvfs included, goes through `crate::fs`. A grep of `std::fs::` outside the seam finds only test code and the `TryLockError` type.
