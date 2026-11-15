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
| 2026-11-15 | R4.5 review | `LLMOBS_MODEL_S3_CASES=8 cargo test -p llmobs-storage --all-features --lib model_tests::storage_matches_the_model_on_s3`, six runs, after the 3 s calm arm and the 25 s window | debug, in-memory bucket | 11 s to 32 s | pass; the default is 8 again |
| | | `just model-s3 200` (the first row planned) | release | | deferred (long run) |
| | | Garage: a lifecycle rule with `AbortIncompleteMultipartUpload` applied and read back (R4.9, `docs/self-hosting/s3.md`) | | | pending (external; needs a Garage server) |

### Findings

None yet. The S3 mode of R4.5 found no disagreement in the 50-case run above.

**M4-1, shutdown retry backoff (to raise with the coordinator).** During shutdown, `flush.rs` `sleep_backoff` is woken by `stop.changed()` only once. A watch receiver that has already seen the change does not wake again, so each later retry sleeps its full backoff (the backoff doubles from 1 s up to 60 s, so 2, 4, 8 and 16 s in turn) until the 30 s cut-off. A flush that fails through a shutdown therefore loses the retries that would have committed it. The proposal is to cap the backoff at about 1 s while stopping. This is a production change, so it is not made here. The model harness works around it: 3 s into a restart it switches the fault schedule off (`CALM_AFTER`), so a retry commits.

## M5 — Resource exhaustion

The resource milestone (plan `docs/plans/r-hardening/05-m5-resources.md`, decision D-R-9). It runs the engine out of disk, memory and file descriptors and checks that each ends in a clean refusal.

What the tests cover:
- **Disk (`model_tests/resources/disk_full.rs`):** D1 appends stop at `min_free` before the disk fills; D2 a full disk fails the WAL and a restart recovers; D3 a flush on a full disk retries without temp files; D4 a compaction that does not fit is deferred; D5 a compaction that meets `ENOSPC` fails cleanly; D6 a spilling query stops at `min_free`; D7 the read cache never fills the disk; D8 `put_media` on a full disk leaves nothing; D9 running out of inodes is an error, not a panic.
- **Spill cap (R5.2):** the spill limit is the free disk above `min_free_bytes`, shared by all queries (unit tests in `disk.rs` and `sql/run.rs`, and `lib.rs`'s `a_spill_past_the_cap_fails_as_too_large`).
- **Memory (`model_tests/resources/memory.rs`):** a child process under `systemd-run --user --scope -p MemoryMax=…`, at the defaults and at a small-machine profile, and the minimum each survives.
- **File descriptors (`model_tests/resources/descriptors.rs`):** a load under a lowered `RLIMIT_NOFILE`, the peak of open descriptors, and opening under a low limit.

Environment:
- **Small filesystem:** an unprivileged `unshare -rm` 64 MiB tmpfs. On Ubuntu 24.04 and later the
  namespace is refused unless `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` is set
  (until the next reboot). Tmpfs misses what a disk does: `fdatasync` is a no-op, so a sync never reports `ENOSPC`, and `remount,size=` fails in the namespace, so the tests free space by removing a ballast file. The ext4 loop device (`LLMOBS_TEST_SMALL_FS`) is not run yet.
- **Memory:** a systemd user scope (`systemd-run --user`). Its limit is honoured by the query pool from R5.6, not before.
- **Cgroup namespace:** the sandbox this plan was written in has a `/proc/self/cgroup` path that starts with `/../..`, which the sysinfo reader cannot resolve. The memory probe (`child_sees_the_scope_limit`) reports whether it applies.

### Runs

| Date | Commit | Command | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | 4c8fe6a | `cargo test -p llmobs-storage --all-features --lib writes_check_free_disk_now`, before the fix | debug | 0.02 s | fail: a block was written on a stale measurement |
| 2026-11-15 | 4c8fe6a | same, after the fix | debug | 0.02 s | pass |
| 2026-11-15 | 4c8fe6a | `read_cache` unit tests | debug | 1.5 s | pass: 19 tests, 1 ignored |
| 2026-11-15 | uncommitted: check per call | D7 under `unshare -rm`, `--ignored --exact` | debug, 64 MiB tmpfs | 5.4 s | fail: sampler minimum 2.7 MiB, below `min_free - 1 MiB` |
| 2026-11-15 | uncommitted: check per block, no lock | D7, same | debug, 64 MiB tmpfs | 6.7 s | fail: sampler minimum 3.7 MiB |
| 2026-11-15 | 4c8fe6a | D7, same | debug, 64 MiB tmpfs | 9.9 s | sampler passes (minimum 15.8 MiB); fails in settle, see M5-6 |
| 2026-11-15 | 4c8fe6a | D1 `appends_stop_at_min_free_before_the_disk_fills`, same | debug, 64 MiB tmpfs | 10.4 s | pass |
| | | `just resources` (three runs), `LLMOBS_TEST_SMALL_FS` on an ext4 loop device | | | pending (long run, not run in this pass) |
| | | `just resources-memory`, `just resources-memory-min defaults` and `small` | release | | pending (long run, needs free RAM and about 10 GiB of disk) |
| | | `peak_at_the_default_limit` and the low-limit descriptor runs | debug | | pending: the run was refused by the sandbox's permission check, so it is not recorded here |

### Measured limits

- **Memory:** pending. The smallest passing limit and the documented minimum for the defaults and the small profile are not measured yet, so spec 03 § 10 gives them as provisional.
- **Descriptors:** pending. The baseline and peak at the default limit, and the lowest limit that runs without errors, are not measured yet.

### Findings

**M5-1, a failed local write left its temp file.** `objects.rs::put_local` returned on a `write_all` error and `publish_local` on a `sync_all` or rename error, without removing `.<name>.tmp-<uuid>`. On a full disk each flush retry left another partial file under a new name, so the freed space was eaten again until a restart. The earlier `SQLITE_FULL` in `collect_staged` on a full disk has the same cause. Confirmed by D8, D3 and D5. Fixed in 1f5e989 (R5.5).

**M5-2, a spill-cap failure mid-merge stops later spills until restart (D6).** Investigation found no leaked reservation: the disk counter and the per-query pools balance after a failed spill (see the new unit test `lib.rs` `a_spill_failure_leaves_no_reservation_behind`, and D6's assertions that the pool and the spill counter reach 0). The likely confounder is the 64 × 16 KiB filler appended between runs 2 and 3, which pushes the sort over the 32 MiB cap, so run 3 fails for a reason other than a leak. To confirm with a D6 run (deferred: it needs a small filesystem). Not fixed in M5.

**M5-3, D5 stops at its free-space assertion.** D5 (compaction that meets `ENOSPC`) fails at its free-space assertion, after catalog writes that follow the ballast. Cause unconfirmed. Not fixed in M5.

**M5-4, a 16 MiB per-query cap refuses a trace list over a tiny project.** Found by R5.7's smoke run: with `query.max_memory` at 16 MiB, a trace list over a tiny project is refused, because spill shares can drop to about 98 KiB. Not fixed in M5; spec 03 § 10 notes it under Memory.

**M5-5, the read cache outran the monitor (D7).** `ReadCache::write_blocks` trusted the monitor's measurement, up to a second old, and several concurrent calls could each pass the check and then write, so the cache took the disk below `min_free_bytes`. The fix measures before each block and serializes the check with the writes (`read_cache.rs`, `write_lock`). The regression test is `read_cache::tests::writes_check_free_disk_now_not_the_last_measurement`, which fails before the change. Fixed in 4c8fe6a (R5.8). The lock was needed: a measurement per call alone still let concurrent calls overshoot.

**M5-6, cache writes ignore compaction reservations (found by D7, open).** After M5-5, D7's sampler passes, and the case then fails in its settle step: a compaction job's Parquet write returns `ENOSPC` after the ballast is removed (the job's input reservation had passed). The read cache checks `free()`, not `free()` minus the outstanding reservations, so cache blocks can take space a compaction has reserved. The cause is suspected, not confirmed. Not fixed in M5: it is a rule question (whether the cache respects reservations) and the case's 64 MiB filesystem may be too small for its settle.

### Requests

- **Track N, `docs/self-hosting/`** (not edited in M5): a sizing section with the memory minimum and small profile, `min_free_bytes` and the descriptor limit; `LimitNOFILE=` in the systemd unit at the recommended value, and an optional `MemoryMax=` (honoured by the query pool since R5.6); Helm `values.yaml` resource requests and limits at the documented minimum.
- **Server and CLI:** `llmobs serve` raises its soft `RLIMIT_NOFILE` to the hard limit at start, as Go programs do, and logs both values (decided, Q2). This is the coordinator's to assign.
- **`/readyz`:** no change requested. It already returns 503 when the WAL has failed (`crates/server/src/http/health.rs`, from `spans_wal_failed`), and spec 12 lists the free-disk check. Revisit it only if the WAL rotation decision changes what counts as a WAL failure.
- **The 422 text:** R5.2 changed `RunError::TooLarge` to "the query needed more memory or temporary disk space than allowed". The server still says "needed more memory than allowed" in `crates/server/src/http/error.rs`, `crates/server/src/alerts/evaluate.rs`, `crates/server/src/api/openapi.rs` and `crates/server/openapi.json`, and `docs/spec/05-query.md` says "needed more memory than the cap". Add "or temporary disk space" to each.
- **WAL rotation (coordinator, Q3):** a proposed decision is written out in `docs/plans/r-hardening/05-m5-resources.md` § R5.8, with the provisional sentence now in spec 03 § 1. The coordinator numbers and records it. Until then `wal/writer.rs` is unchanged.

---

## M6 — Concurrency

Track R, plan `docs/plans/r-hardening/06-m6-concurrency.md`. The sub-phases add a concurrent mode to the model test: background flush, compaction and GC run for real while reader threads check every read against the model.

### Findings

**M6-1, GC deleted a file a pre-raise snapshot still read.** A deletion pass raises a clean file: `Catalog::apply` replaces its `FileMeta` with a new `Arc`. A snapshot taken before the raise holds the old `Arc`, but when the file was later removed, the committer retired only the new `Arc`, so GC's in-use check (`strong_count`) did not count the reader. Once the grace had passed, GC deleted the object under the reader. The production grace of 5 minutes hid it. The regression test is `gc::tests::gc_keeps_a_file_a_snapshot_from_before_its_raise_reads`, which fails before the change at its GC step. Class: storage. Fixed in R6.4 (commit 5b01647): the committer keeps a `Weak` of each replaced version (`superseded`), and GC treats the file as in use while one is alive.

---

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
