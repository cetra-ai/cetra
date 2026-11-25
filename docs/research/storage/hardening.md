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
| M1-3 | `regression_applied_deletion_does_not_hide_a_later_trace` (found by `just model 200`, interval mode, shrunk seed `cc 8c65b9dc1383df0cdf6ac932ac67336a922d112de681e5b64408161734df1a3e`) | The `traces` gate was built from every pending deletion of the project, not from the ones the file had not applied. A traces partial has no ingest time, so a deletion the file had already applied hid a trace of the same id ingested after it, while another deletion of the project was pending. Transient: no data lost. | storage | commit a6f36cb `storage: gate a traces file only by the deletions it has not applied` (groups of gated files keyed by their pending deletions). The same bug also made `model_tests::hook_crash::every_model_hook_crash_reads_back` case 19 fail: a re-ingested trace was missing after an aborted ApplyDeletions |

## M2: Durability

The durability milestone (plan `docs/plans/r-hardening/02-m2-durability.md`, decisions D-R-5, D-R-6 and D-R-13). Every local file call of `crates/storage` goes through the seam `src/fs.rs`, statvfs included. Lossy mode (`src/fs/lossy.rs`, test-only) records each registered file's synced and unsynced writes and, at a crash, applies one of three modes chosen by a seed: `DropAll`, `KeepRandom` and `TearLast`. Lossy crashes run in the model test (R2.9), in the hook-abort children (R2.10), in the model's hook crash (R2.11) and in unit tests of each write path. The LazyFS cross-check (`tools/lazyfs/`, R2.12) runs the crash children on the real binary. It is pending (see § LazyFS and the shim).

### Runs

| Date | Commit | Command | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | 7a7bae5 | `CETRA_HOOK_CRASH_CASES=52 cargo test -p cetra-storage --all-features --lib -- --ignored --exact model_tests::hook_crash::every_model_hook_crash_reads_back` | debug, local | 102 s | pass: 52 cases, 44 aborted at their hook point, 8 completed without reaching it (cases 14, 18, 19, 41 to 45); 26 lossy cases among them |
| 2026-11-15 | 7a7bae5 | the same, default count (25) | debug, local | 52.8 s | pass: 25 cases, 22 aborted, 3 completed (14, 18, 19) |
| | | R2.9 long run, R2.10 with `CETRA_LOSSY_SEEDS=10`, R2.11 with `CETRA_HOOK_CRASH_CASES=500`, `just lazyfs` three times, the GitHub FUSE probe | | | pending (R2.14) |

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

## M3 — Fuzzing

Track R, plan `docs/plans/r-hardening/03-m3-fuzzing.md`. The fuzz targets live in `fuzz/`; a crash is fixed with its input in `fuzz/regressions/<target>/`.

### Findings

| Id | Target | Regression | Cause | Classification | Severity | Fix |
|---|---|---|---|---|---|---|
| M3-1 | `wal_record`, `wal_replay` | `fuzz/regressions/wal_record/crash-fadaf511…`, `fuzz/regressions/wal_replay/crash-f945199…` | A CRC-valid WAL record declared a huge LZ4 decompressed size in a buffer prefix; arrow-ipc allocated it unchecked, so replay aborted the process on every start. | ours (storage) | medium: needs a CRC-valid corrupt record | commit 1b3311d plus this commit (R3.8) |
| M3-2 | `parquet_file` | `fuzz/regressions/parquet_file/min-crash-3c48037…` | A 21-byte file whose footer claims more metadata than the file holds. Upstream's push decoder underflows (parquet `push_decoder.rs:391`). The guard catches it, so every read reports the file corrupt; the upstream report is not filed yet. | upstream guarded | low: the file must be corrupt already | the regression test in `verify.rs` (R3.8) |

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
| 2026-11-15 | R4.5 | `CETRA_MODEL_S3_CASES=8 cargo test -p cetra-storage --all-features --lib model_tests::storage_matches_the_model_on_s3` | debug, in-memory bucket | 32.7 s | pass |
| 2026-11-15 | R4.5 | `CETRA_MODEL_S3_CASES=50 CETRA_MODEL_S3_STATS=1 cargo test -p cetra-storage --all-features --lib model_tests::storage_matches_the_model_on_s3` | debug, in-memory bucket | 279.5 s | pass: 50 cases, 51 injected faults, 446 puts, 325 multipart completes, 29 aborts, 11 abandoned uploads, 0 `FaultyRead` errors |
| 2026-11-15 | R4.5 | `CETRA_MODEL_S3_CASES=4 cargo test -p cetra-storage --all-features --lib model_tests::storage_matches_the_model_on_s3`, nine runs | debug, in-memory bucket | 5.7 s to 98 s (8 of 9 under 45 s) | pass; the default is now 4. At 8 cases five runs took 10 s to 103 s. A shutdown whose flush fails under faults waits out its 30 s grace period |
| 2026-11-15 | R4.5 review | `CETRA_MODEL_S3_CASES=8 cargo test -p cetra-storage --all-features --lib model_tests::storage_matches_the_model_on_s3`, six runs, after the 3 s calm arm and the 25 s window | debug, in-memory bucket | 11 s to 32 s | pass; the default is 8 again |
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
  (until the next reboot). Tmpfs misses what a disk does: `fdatasync` is a no-op, so a sync never reports `ENOSPC`, and `remount,size=` fails in the namespace, so the tests free space by removing a ballast file. The ext4 loop device (`CETRA_TEST_SMALL_FS`) ran on the VM below (`truncate -s 128M`, `mkfs.ext4 -q`, `sudo mount -o loop`, 95 MiB free).
- **The VM (2026-11-21 runs):** ubuntu-vm, Ubuntu 26.04, 12 vCPUs, 94 GB RAM, a 500 GB virtual disk on the host's NVMe; `kernel.apparmor_restrict_unprivileged_userns=0`. A shared VM, not a dedicated machine.
- **Memory:** a systemd user scope (`systemd-run --user`). Its limit is honoured by the query pool from R5.6, not before.
- **Cgroup namespace:** the sandbox this plan was written in has a `/proc/self/cgroup` path that starts with `/../..`, which the sysinfo reader cannot resolve. The memory probe (`child_sees_the_scope_limit`) reports whether it applies.

### Runs

| Date | Commit | Command | Mode | Duration | Result |
|---|---|---|---|---|---|
| 2026-11-15 | 4c8fe6a | `cargo test -p cetra-storage --all-features --lib writes_check_free_disk_now`, before the fix | debug | 0.02 s | fail: a block was written on a stale measurement |
| 2026-11-15 | 4c8fe6a | same, after the fix | debug | 0.02 s | pass |
| 2026-11-15 | 4c8fe6a | `read_cache` unit tests | debug | 1.5 s | pass: 19 tests, 1 ignored |
| 2026-11-15 | uncommitted: check per call | D7 under `unshare -rm`, `--ignored --exact` | debug, 64 MiB tmpfs | 5.4 s | fail: sampler minimum 2.7 MiB, below `min_free - 1 MiB` |
| 2026-11-15 | uncommitted: check per block, no lock | D7, same | debug, 64 MiB tmpfs | 6.7 s | fail: sampler minimum 3.7 MiB |
| 2026-11-15 | 4c8fe6a | D7, same | debug, 64 MiB tmpfs | 9.9 s | sampler passes (minimum 15.8 MiB); fails in settle, see M5-6 |
| 2026-11-15 | 4c8fe6a | D1 `appends_stop_at_min_free_before_the_disk_fills`, same | debug, 64 MiB tmpfs | 10.4 s | pass |
| 2026-11-15 | a1f5ce5 | `just resources` (log `resources-3`) | debug | 53 s | 4 pass, 11 fail, before the fixes. Causes: triage §2 |
| 2026-11-15 | a1f5ce5 + 16927ae | `just resources` | release | 372 s | 15/15 pass |
| 2026-11-15 | a1f5ce5 + 16927ae | `resources-memory` small profile, a systemd scope per limit | release | | see Measured limits. The small profile ran in an instrumented copy (sampler thread), not the committed code |
| earlier | a1f5ce5 | `resources-memory-min defaults` (vm-report-1) | release | 400.7 s | 8 GiB passes, peak RSS 7.40 GiB |
| 2026-11-21 | 041b2b1 | `just resources` filters, three times (`-p cetra-storage --lib`, the recipe's test names) | release, VM, 64 MiB tmpfs under `unshare -rm` | 407 s, 408 s, 390 s | 15/15 pass each time |
| 2026-11-21 | 041b2b1 | same, `CETRA_TEST_SMALL_FS` on a 128 MiB ext4 loop device (`small_fs::`, `disk_full::`) | release, VM, ext4 (95 MiB free) | 110 s | 9/10: D3 fails, deterministic (4 of 4 reruns, also with `commit=1`): 192,512 bytes free, below its 192 KiB bound. See M5-9 |
| 2026-11-22 | 041b2b1 + D3 fix | D3 on the ext4 loop device, three times; D3 on tmpfs | release, VM | 12 s, 11 s, 11 s (one wait on a build lock); 9 s | pass (4/4) |
| 2026-11-22 | same | `small_fs::` and `disk_full::` on the ext4 loop device | release, VM | 92 s | 10/10 pass |
| 2026-11-22 | same | `a_low_limit_gives_errors_not_panics` at limits 1024 and 512 (a temporary change of the list, not committed) | release, VM | 92 s | pass: no panic, no wedge, no `WalFailed`; peak 117 (1024) and 102 (512), baseline 16; queries refused as too large under the 128 MiB cap |
| 2026-11-22 | same | `peak_at_the_default_limit` (soft limit 65,536) | release, VM | 64 s | pass: baseline 16, peak 109 (101 files) |
| 2026-11-22 | same | `just resources-memory` | release, VM | 94 s | 2/3: the probe sees the scope limit (`0::/user.slice/…/run-….scope`); defaults pass at 10.5 GiB (peak RSS 8.22 GB, 90 queries too large, no `Busy`); the small profile is killed at its constant `MIN_MEMORY_SMALL` (1,280 MiB), as § Measured limits expects |
| 2026-11-22 | same | `just resources-memory-min defaults` | release, VM | 384 s | 8 GiB is the smallest passing limit (peak RSS 7.92 GB); 7.75, 7.5 and 7 GiB are killed |
| 2026-11-22 | same | `just resources-memory-min small` | release, VM | 9 s | no result: 2 GiB, the largest limit in the list, is killed |
| 2026-11-22 | same + the compaction fix | `resources-memory-min small` with the list 4, 3, 2.5, 2 GiB, then 8, 6, 5, 4 GiB (temporary changes, not committed) | release, VM | 39 s, 381 s | 4 GiB killed in both; 8, 6 and 5 GiB pass (peak RSS 4.97, 4.19, 5.01 GB); the search's 4.25 GiB passes (peak RSS 4.45 GB), the smallest passing limit |
| 2026-11-15 | a1f5ce5 + 16927ae | `peak_at_the_default_limit` (soft limit 65,536, 30 s, 4 appenders, 4 query loops) | | 30 s load | peak 189 (116 in the first run), baseline 16 to 17. Peak composition from an instrumented copy |
| 2026-11-15 | a1f5ce5 + 16927ae | `a_low_limit_gives_errors_not_panics`, limits 256, 128, 64, 32 | | 20 s each | all run, no panic, no wedge |
| 2026-11-15 | a1f5ce5 + 16927ae | `opening_under_a_low_limit_is_a_clean_error`, limits 64, 32, 16, 8 | | | 64, 32 and 16 open; 8 fails cleanly (`io`) |

### Measured limits

- **Memory, defaults:** smallest passing limit 8 GiB, peak RSS 7.40 GiB at 8 GiB. The kill point is between 7.75 and 8 GiB. Re-run on the VM (2026-11-22, release, glibc): the same, 8 GiB passes with peak RSS 7.92 GB and 7.75 GiB is killed.
- **Memory, small profile (glibc, the test binary's allocator):** 4 GiB passes (VmHWM 4.02 GiB); 2 GiB is killed. On the VM (2026-11-22): 4 GiB is killed and 4.25 GiB passes (peak RSS 4.45 GB), so the minimum is about 4 to 4.5 GiB, and `MIN_MEMORY_SMALL` (1,280 MiB) makes `small_profile_at_its_minimum` fail until M5-7 and M5-8 settle the documented value. With `MALLOC_ARENA_MAX=2` and 1 MiB trim and mmap thresholds: 2 GiB passes (VmHWM 1.56 GiB), 1.25 GiB is killed. With mimalloc, in an earlier build (`eabb4d1`): 4 GiB passes (VmHWM 3.21 GiB), 2 GiB is killed. The shipped binary uses mimalloc, so the glibc figures are not the shipped ones (M5-8). The documented minimum is 2 to 4 GiB until M5-7 and M5-8 are settled, and spec 03 § 10 says so.
- **Descriptors:** on the VM (2026-11-22), baseline 16 and peak 109 at 65,536; limits 1024 and 512 run without any error but queries refused as too large (peaks 117 and 102). Earlier: baseline 16 to 17 at the soft limit 65,536. Peak 189 with a 128 MiB query cap, 166 of them spill files (116 in the first run). This is the "before" number for `with_max_spill_merge_fan_in(16)`, which is not applied: the test never nears 1024. Limits 256, 128, 64 and 32 all run, with clean errors (no lowest limit ran without errors). At 32 the WAL fails and a reopen recovers. Opening runs at 16 and above and fails cleanly at 8.

### Findings

**M5-1, a failed local write left its temp file.** `objects.rs::put_local` returned on a `write_all` error and `publish_local` on a `sync_all` or rename error, without removing `.<name>.tmp-<uuid>`. On a full disk each flush retry left another partial file under a new name, so the freed space was eaten again until a restart. The earlier `SQLITE_FULL` in `collect_staged` on a full disk has the same cause. Confirmed by D8, D3 and D5. Fixed in 1f5e989 (R5.5).

**M5-2, a spill-cap failure mid-merge stops later spills until restart (D6).** Investigation found no leaked reservation: the disk counter and the per-query pools balance after a failed spill (see the new unit test `lib.rs` `a_spill_failure_leaves_no_reservation_behind`, and D6's assertions that the pool and the spill counter reach 0). The likely confounder is the 64 × 16 KiB filler appended between runs 2 and 3, which pushes the sort over the 32 MiB cap, so run 3 fails for a reason other than a leak. To confirm with a D6 run (deferred: it needs a small filesystem). Not fixed in M5.

**M5-3, D5 stops at its free-space assertion.** D5 (compaction that meets `ENOSPC`) fails at its free-space assertion, after catalog writes that follow the ballast. Cause unconfirmed. Not fixed in M5.

**M5-4, a 16 MiB per-query cap refuses a trace list over a tiny project (open).** Found by R5.7's smoke run. Cause: DataFusion keeps `sort_spill_reservation_bytes` (10 MiB by default) per sort partition that cannot spill, so 8 partitions need about 80 MiB. The model's own trace read of the seed is refused before any load. The descriptor tests now use a 128 MiB cap (16927ae). Proposed engine change, not done: the session sets `sort_spill_reservation_bytes` to `min(10 MiB, max_memory / (4 × partitions))`, so small caps work. Spec 03 § 10 notes it under Memory.

**M5-5, the read cache outran the monitor (D7).** `ReadCache::write_blocks` trusted the monitor's measurement, up to a second old, and several concurrent calls could each pass the check and then write, so the cache took the disk below `min_free_bytes`. The fix measures before each block and serializes the check with the writes (`read_cache.rs`, `write_lock`). The regression test is `read_cache::tests::writes_check_free_disk_now_not_the_last_measurement`, which fails before the change. Fixed in 4c8fe6a (R5.8). The lock was needed: a measurement per call alone still let concurrent calls overshoot.

**M5-6, cache writes ignore compaction reservations (found by D7, open).** After M5-5, D7's sampler passes, and the case then fails in its settle step: a compaction job's Parquet write returns `ENOSPC` after the ballast is removed (the job's input reservation had passed). The read cache checks `free()`, not `free()` minus the outstanding reservations, so cache blocks can take space a compaction has reserved. The cause is suspected, not confirmed. Not fixed in M5: it is a rule question (whether the cache respects reservations) and the case's 64 MiB filesystem may be too small for its settle. Seen again in `the_read_cache_never_fills_the_disk`: its settle compacts 42 MiB of filler on the same disk while the default read cache (about 48 MiB) holds blocks. The test now sets the read cache to 1 MiB before the settle (a test fix, not an engine fix), so the engine question stays open.

**M5-7, most resident memory during ingest is outside the memory budgets (open).** In an ingest-only run the process grows about 70 MiB/s while the memtable stays at 50 to 130 MiB and is flushed. The accounted pools (memtable at most 130 MiB, query pool at most about 170 MiB) do not explain it. glibc arenas account for a large part (`MALLOC_ARENA_MAX=2` halves the peak). Not located. The next step is a heap profile (dhat or jemalloc profiling). Not fixed.

**M5-8, the memory test measures glibc, the shipped binary uses mimalloc (D56, open).** The storage test binary uses glibc malloc, and `crates/cli/src/main.rs` sets mimalloc. The memory numbers above are glibc numbers. The earlier mimalloc build was no better (killed at 2 GiB, 3.0 GiB anon at 4 GiB). The fix is to set the same allocator in the memory test (a dev-dependency and `#[global_allocator]` in the test cfg, as the cetra-m5 experiment did), then re-measure. Not done.

**M5-9, a failed flush attempt keeps the files it did write until GC (found on ext4 by D3; test fixed, engine question open).** On the ext4 loop device D3 ends with 192,512 bytes free, below its bound of 192 KiB, every time; on tmpfs it passes by margin. No temp file is left (R5.5 holds). A capture of the tree during the case shows what took the space: each failed attempt of the filler's flush writes the filler's traces file (9 KB, 12 KiB on disk) before its spans file meets `ENOSPC`, and that file stays under its final name, staged (about 8 KiB of `catalog.db-wal` per attempt as well). That is spec 03 §2: "The staged names of a failed attempt stay for GC", and GC takes staged rows older than 1 h. So the case asserted more than the spec promises: it now counts the bytes of the staged files with the free space (`disk_full.rs`, D3). The engine question stays open: on a nearly full disk, every retry (backoff capped at 60 s) keeps whatever outputs fitted for at least an hour, so the retries can take back the space that was freed. Removing the written outputs of a failed attempt (best effort, the staged rows staying for an ambiguous upload) would change spec 03 §2, so it is reported, not done.

### Requests

- **Track N, `docs/self-hosting/`** (not edited in M5): a sizing section with the memory minimum and small profile, `min_free_bytes` and the descriptor limit; `LimitNOFILE=` in the systemd unit at the recommended value, and an optional `MemoryMax=` (honoured by the query pool since R5.6); Helm `values.yaml` resource requests and limits at the documented minimum.
- **Server and CLI:** `cetra serve` raises its soft `RLIMIT_NOFILE` to the hard limit at start, as Go programs do, and logs both values (decided, Q2). This is the coordinator's to assign.
- **`/readyz`:** no change requested. It already returns 503 when the WAL has failed (`crates/server/src/http/health.rs`, from `spans_wal_failed`), and spec 12 lists the free-disk check. Revisit it only if the WAL rotation decision changes what counts as a WAL failure.
- **The 422 text:** R5.2 changed `RunError::TooLarge` to "the query needed more memory or temporary disk space than allowed". The server still says "needed more memory than allowed" in `crates/server/src/http/error.rs`, `crates/server/src/alerts/evaluate.rs`, `crates/server/src/api/openapi.rs` and `crates/server/openapi.json`, and `docs/spec/05-query.md` says "needed more memory than the cap". Add "or temporary disk space" to each.
- **WAL rotation (coordinator, Q3):** a proposed decision is written out in `docs/plans/r-hardening/05-m5-resources.md` § R5.8, with the provisional sentence now in spec 03 § 1. The coordinator numbers and records it. Until then `wal/writer.rs` is unchanged.

---

## M6 — Concurrency

Track R, plan `docs/plans/r-hardening/06-m6-concurrency.md`. The sub-phases add a concurrent mode to the model test: background flush, compaction and GC run for real while reader threads check every read against the model.

### Findings

**M6-1, GC deleted a file a pre-raise snapshot still read.** A deletion pass raises a clean file: `Catalog::apply` replaces its `FileMeta` with a new `Arc`. A snapshot taken before the raise holds the old `Arc`, but when the file was later removed, the committer retired only the new `Arc`, so GC's in-use check (`strong_count`) did not count the reader. Once the grace had passed, GC deleted the object under the reader. The production grace of 5 minutes hid it. The regression test is `gc::tests::gc_keeps_a_file_a_snapshot_from_before_its_raise_reads`, which fails before the change at its GC step. Class: storage. Fixed in R6.4 (commit 5b01647): the committer keeps a `Weak` of each replaced version (`superseded`), and GC treats the file as in use while one is alive.

### Pending and limits

- **Nightly stress.** `just stress` (R6.7) is not yet run nightly. The nightly steps wait for M3's nightly workflow (`.github/workflows/nightly.yml`); until it exists, the stress test runs only by hand.
- **Read cache (R6.8).** `read_cache::tests::concurrent_use_never_returns_wrong_bytes` pins no wrong bytes, the capacity and the file-to-index direction under 8 threads. A block can be indexed without its file for a moment when an eviction runs between a block's write and its index insert (benign: the next read finds the file missing, drops the entry and fetches again). The test does not reach the window that `write_blocks` guards by removing a block written after its object was evicted: with that removal taken out it still passed 200 of 200 runs, so that guard had no deterministic test. `read_cache::tests::a_block_written_after_its_object_was_evicted_is_not_indexed` now covers it through a test-only hook between the block's write and its index insert: removing the guard fails it.
- **Gate and flush races (R6.8).** The snapshot tests pin the gate against media collection, finish_drops and obsolete collection (each removal fails a test, checked by hand). One of them passes with the gate removed and needs a stronger scenario: `a_snapshot_matches_its_copy_under_concurrent_commits` (the snapshot's retired `Arc`s keep listed files in use). `the_gate_holds_a_quarantined_file` now drops the project after the verify pass, which retires the quarantined file as an obsolete row, so removing the gate fails it. `flush_now`: removing the force flag keeps `flush_now_covers_every_acknowledged_append` green but makes it take about 60 s instead of 1 s, and removing the shutdown checks keeps `flush_now_racing_shutdown_fails_cleanly` green, because the flush finishes before shutdown wins. `flush_now_gives_up_once_shutdown_began` holds the flush and fails when the in-loop shutdown check is removed. The M1 model has no `Flush` op, so B2's `flush_now` is not exercised by the model: a follow-up for the model.
- **Watchdog scope.** The stuck-item watchdog covers `run_segment` only. `check_full`, the barriers and `settle_concurrent` run outside it, so a hang there is not caught by the watchdog.

---

## M7 — Scale and soak

What M7 covers:

- realistic payloads for the benchmarks and the soak, from a permissively licensed corpus (R7.2);
- the ingest benchmark on those payloads, with the measured compression of the Parquet files (R7.3);
- the 5M-trace benchmark on those payloads, local and on S3 (R7.4);
- the soak: its driver, its per-minute samples, and its pass/fail rules with the sampled-trace oracle (R7.5, R7.6);
- the soak's plots and recipes (R7.7);
- the 24 h dry run, and the 72 h soak on a dedicated server, recorded in `soak.md` (R7.7, R7.8).

### Runs

| date | commit | run | machine | result | write-up |
|---|---|---|---|---|---|
| 2026-11-22 | 041b2b1 | soak calibration, 1 h at 2,000 spans/s, local | VM, virtual disk | fail: `rss_max`, `ingest_rate`, `settled` (M7-1, M7-2, M7-3) | `soak.md` § The VM dry run |
| 2026-11-22 | 041b2b1 + #214 | soak `20261122-vm-48h`, 48 h at 1,000 spans/s, local, `retention_days` 0 | VM, virtual disk | 25/28; fail: `rss_growth`, `rss_max`, `ingest_rate` (M7-2, M7-4) | `soak.md` § 20261122-vm-48h |
| 2026-11-24 | b0e5710 | soaks `20261124-vm-noq-12h` and `20261124-vm-q-12h`, 12 h at 1,000 spans/s side by side, without and with 1 query/s | VM, virtual disk | both fail `rss_max`, `ingest_rate` (M7-2, M7-4) | `soak.md` § 20261124 |

The benchmark runs 8–14 and the 72 h soak on dedicated hardware are run by hand and not made yet. See `benchmarks.md` § Track R M7 runs and `soak.md` § Runs.

### Findings

**M7-1, a day close of large spans failed every pass with `ResourcesExhausted` (found by the soak calibration on the VM; fixed).** The 1 h calibration (2,000 spans/s, 2026-11-22) failed its `settled` rule: `compaction still busy after 50 passes: 0 jobs, 1 failed`. The failing job was the `spans` day close of the smallest project, 17 inputs, every pass (100 times), logged only as `error = "read"`. The stopped 400 spans/s run had the same kind of failure, one or two L0 merges every 16 minutes, which a later pass got through. A log of the DataFusion variant (b510213, `source` in the job failure line, no text) and a rerun of the job on a copy of the calibration's data directory showed `resources_exhausted`: the merge reads 8,192 rows per batch from each input, and at about 4.6 KB per span that is about 38 MB per input, so 17 inputs pass the 1 GiB `compaction.max_memory` pool. With realistic payloads a large day never closes. Class: storage. Fixed in 5d49a1e (`compaction/merge.rs`, `batch_rows`): a merge lowers its rows per batch so that four batches of every input fit in its share of the pool (`max_memory / compaction.threads`), estimating a row at 8 times its stored bytes per row, between 64 and 8,192 rows; spec 03 §5 says so. Regression tests: `compaction::merge::tests::a_merge_of_large_rows_fits_the_compaction_pool` (8 inputs of 512 spans of 8 KiB under a 16 MiB pool; `ResourcesExhausted` before the change) and `batch_rows_follow_the_pool_share_and_the_row_size`. The same job on the copy commits in its first pass with the fix (and with a fixed 1,024-row batch).

**M7-2, RSS grows past the soak's 8 GiB bound (open).** The calibration peaked at 8.49 GiB (rule `rss_max` fails) and the stopped 400 spans/s run reached 10.9 GB after 9 h, with the memtable at 0.25 GB and no read cache. The 48 h run at 1,000 spans/s confirmed the growth: `rss_growth` fails (p95 per quarter 12.6, 13.2, 15.0, 15.2 GB) and RSS peaked at 14.47 GiB. It rises in steps (about 10 h, 12.5 h, 16 h, 36 h), two of them around 22:30 UTC when the day's partitions are near their largest, and it does not fall when retention halves the data. The memtable stays under 314 MiB, the WAL under 200 MiB, local mode has no read cache, and the query pool is bounded, so the growth is outside the storage's budgets (M5-7's memory, seen at a steady load). The 12 h runs of 2026-11-24 split it: file-backed memory stays under 0.1 GB, so the growth is heap. Without queries RSS climbs 6.9 → 9.0 GB in 12 h; with 1 query/s it reaches 12.4 GB by 4 h and then holds at 12.4–12.8 GB, which looks like an allocator high-water mark from the queries' peaks rather than a leak. Not located. Then a harness flaw turned up: the soak's test binary used glibc malloc while the `cetra` binary uses mimalloc (D56), and glibc's per-thread arenas keep freed memory, a known cause of RSS that steps up and does not fall. The test binary now uses mimalloc (`tests/it/main.rs`); the next VM run says how much of M7-2 was glibc's. Next, if RSS still grows: a smaller query budget, and a heap profile of the climb without queries.

**M7-3, the VM does not sustain 2,000 spans/s (capacity).** In the calibration the ingest ratio fell from 0.989 to 0.973 of target over the hour (`ingest_rate` fails), with the process at about 1.24 cores on average and no backpressure. Not investigated further: the 48 h run uses 1,000 spans/s for the disk budget anyway (`soak.md`).

**M7-4, ingest falls behind as the retained data grows (open).** In the 48 h run at 1,000 spans/s the hourly ingest ratio fell from 0.980 to 0.926 over the first day, jumped to 0.965 after the retention drop, fell to 0.904 by 41 h, and jumped to 0.955 after the second drop (`ingest_rate` fails in every window). Process CPU (1.1 → 2.3 cores per hour) and the metrics query's p95 (0.36 → 2.8 s) follow the same saw-tooth with the objects on disk. There was no backpressure, fsync stayed around 3.7 ms, and the 8 Tokio workers ran at about 30% each, so the driver's paced appends do not get enough time rather than storage refusing them. Queries share the runtime with ingest and scan more data as the day grows; the 12 h runs of 2026-11-24 show they cause most of it. Without queries the hourly ratio stays at 0.99 or above for 7 h and bottoms at 0.988; with 1 query/s it falls to 0.93 by 10 h, as in the 48 h run, while the process uses under 2 of 12 cores and never hits backpressure. So the appends wait on something the queries hold or occupy (the shared runtime's workers, or a lock on the append path), not on CPU. Then a harness flaw turned up: each ingest tick added 100 ms of the rate to its budget whatever the time since the last tick, and a late tick was delayed, not caught up, so any delay of an ingest task was lost for good and `ingest_rate` measured the driver's jitter. The budget now follows the clock, and the samples record the append latency (p50, p99, max) and the ticks' lateness, which tell a slow append from a busy runtime. Class: harness for the lost spans; whether queries also slow the appends themselves (storage) is for the next VM run's `appends` samples.

## M8 — Format stability

Track R, plan `docs/plans/r-hardening/08-m8-format.md`. The sub-phases make every persisted format carry a version marker that an older binary checks before it reads or changes anything, and add the golden data directories that every later release must keep opening (D-R-12, D-R-26 to D-R-32, spec 03 § 12).

What M8 covers:

- the one format number of the data directory, in `catalog.db` (R8.2), refused before a migration, a stamp or a write;
- a format header in every WAL segment (R8.3), and the Parquet files' format key (R8.4);
- catalog migration tests and pinned object and cache layouts (R8.5);
- the golden directory generator and its check (R8.6, R8.7), and the release rule with `just check-golden` (R8.8).

### What exists today

Checked on `main` 30bed18 and `claude/r-m3-fuzzing`, before M8. Every persisted format, its version marker, and what a reader does with a version it does not know.

| Format | Where | Marker today | Unknown or newer input today |
|---|---|---|---|
| **WAL segment** | `wal/<table>/<first seq, 20 digits>.wal` (`wal/mod.rs::segment_name`) | **None.** No segment header: a segment is records only (`wal/segment.rs::SegmentWriter::create` writes nothing). The spec 12 sentence "the WAL carries a version byte in each segment header" is not true. | A record with `kind != 1` is `Decoded::Invalid` (`wal/record.rs`). In the **last** segment `replay.rs::scan_segment` treats that as a torn tail and **truncates it** (`truncate_tail`): an older binary erases a newer binary's records. Earlier segments: `WalError::Corrupt` (the open fails), or a skip with `--wal-skip-corrupt`. A payload that is not one batch of the table's current schema is `Corrupt` too (`read_batch`). |
| **WAL record** | inside a segment, `wal/record.rs` | `kind` byte (1 = row batch, 2 reserved). Header 17 bytes (`HEADER_LEN`): `u32 payload_len`, `u32 crc32c`, `u64 seq`, `u8 kind`. Payload: an Arrow IPC stream, LZ4-frame compressed, one batch. Cap: `MAX_PAYLOAD` 256 MiB. | As above. On `claude/r-m3-fuzzing` (to merge before M8): `MAX_DECODED_BYTES` 1 GiB declared decompressed bytes per record, LZ4 ratio bound 256 plus 64 bytes of slack per buffer, **LZ4_FRAME only** (D-R-25 there). These are part of what a v1 record is. |
| **Parquet file** | `objects/tables/<table>/project=<id>/day=<YYYY-MM-DD>/<level>-<uuidv7>.parquet` (`objects.rs::table_file_path`) | KV metadata written by `parquet.rs::file_metadata`: `cetra.table`, `cetra.schema_version` (`"1"`), `cetra.level`, `cetra.min_time`, `cetra.max_time`, `cetra.min_ingest_time`, `cetra.max_ingest_time`. **`cetra.schema_version` is the row normalization version (spec 01), not a file-format version.** | **No code reads any of these keys** (the catalog holds the same bounds). Extra columns are ignored by DataFusion's schema adapter (spec 03 §3). A changed column type or a missing required column is a read error, which makes the file a suspect (`verify.rs::may_be_corrupt`); `verify_file` then finds it corrupt and **quarantines it**: a newer file that is not corrupt would be hidden from queries. |
| **`catalog.db`** | `<data_dir>/catalog.db`, SQLite WAL mode, `synchronous=FULL` | `sqlx` migrations `0001`–`0007` in `crates/storage/migrations/catalog/`, tracked in `_sqlx_migrations` (version, checksum). Copies keep their number: `catalog.db.pre-m<N>` (`catalog/db.rs::pre_migration_copy`, D-N-22). | An applied migration the binary does not know: `sqlx` `MigrateError::VersionMissing`, so the open fails with `CatalogError::Migrate` and a message about a missing migration, before any write (no pre-migration copy: `applied >= expected`). An edited migration file: `VersionMismatch` (checksum). A TEXT value the binary does not know (`data_files.state`, `table_name`, `deletions.kind`, `projects.state`): `CatalogError::Corrupt` while loading. **No marker for a change that adds no migration**, such as a new meaning for a column. |
| **Catalog snapshot** | `Storage::snapshot_catalog` (`snapshot.rs`), `VACUUM INTO` | Same as `catalog.db`. | Same. The backup `manifest.json` is track N's, not storage's. |
| **Object paths** | `data_files.path` (relative path, never parsed), `media/<project>/<sha256 hex>` (`objects.rs::media_path`) | None. | A path is opaque to readers; a *new layout* only applies to new files. |
| **Media blob** | `objects/media/...`, the raw bytes; `media` table holds mime, size, times | None. | None needed: the blob is the bytes. |
| **S3 read cache** | `cache/<file name>/size` (8 bytes LE size, then CRC-32C: `SIZE_FILE_LEN` 12, D-R-21) and `cache/<file name>/<block index>` (block bytes then CRC-32C) (`read_cache.rs`) | None. Block size `BLOCK` = 1 MiB is implicit. | A `size` file that does not parse drops the whole directory, a bad block CRC drops the block: the cache heals itself. **A changed `BLOCK` would not be detected**: old blocks keep valid CRCs and would be served at the wrong offsets. |
| **`LOCK`, `tmp/`, `admin.sock`** | | no content that survives a restart | not formats |
| **`meta.db`, `secret.key`, backup `manifest.json`** | server and track N | their own | out of M8 (§ Requests) |

Two consequences drove the design: an older binary could destroy newer data in two places (the WAL tail truncation and the quarantine of a newer Parquet file), so the refusal must come before either, from a marker in `catalog.db`, which `Storage::open` reads first.

### Findings

**M8-1, an older binary truncated a newer WAL tail.** A record whose `kind` it does not know is a torn tail in the last segment, and `replay.rs` truncated it. Fixed in R8.3 (commit fae9f25a): every segment starts with a format header, and an open refuses a newer segment before replay, so nothing is truncated. The tests are in `wal/segment.rs` and `wal/replay.rs`.

**M8-2, an older binary quarantined a newer Parquet file.** A file the old reader cannot read is a suspect, and `verify_file` quarantined it, so a newer file that was not corrupt was hidden from queries. Fixed in R8.2 (commit 26a9b1e3): the directory's format marker in `catalog.db` refuses a newer directory before anything is verified, migrated, stamped or written.

**Review follow-ups.** Commit e51bbae9 (R8 review) syncs a reused WAL segment header. Commit 2d9872fe (R8 review) makes the refusals read-only and runs them before anything is written: the catalog check, then every WAL segment header. A refused open changes no existing file's content (spec 03 § 12).

**Golden directory size.** The scenario of `golden_tests/build.rs` writes 21 files, 363,850 bytes, measured after the error and payload checks were added (R8.6 review fixes; 359,268 bytes before). The limit is 1 MiB (`golden_directories_stay_small`); each release adds its directory to the repository.

### Commands and tests

- `just golden-dir <name>`: writes `fixtures/storage/golden/<name>` from the current build and appends it to `RELEASES`, unless the name starts with `scratch` (not appended). A name is `scratch...` (letters, digits, `-`, `_`) or `X.Y.Z` with digits only; the recipe refuses any other name (a `/`, a `..`, an rc tag) before anything is written. The name must be the workspace version (`CARGO_PKG_VERSION`), which `release-prep` sets; the test refuses another name before anything is written.
- `just check-golden`: fails a branch that deletes a released directory or removes a `RELEASES` line, against `origin/main`'s merge base. Skipped without `origin/main`; `just check-rust` runs it. CI does not run it yet (the ci.yml request is open).
- `golden_tests::` (`crates/storage/src/golden_tests/`): `build_is_deterministic`, `build_leaves_the_expected_shape`, `a_fresh_golden_directory_opens` (the check runs while no directory is committed), `the_committed_golden_directories_open`, `golden_directories_stay_small`, `a_newer_golden_directory_is_refused`, `every_released_version_has_a_golden_directory`, `every_golden_directory_is_released`, and the ignored `write_golden_dir`.

### Pending

- No golden directory is committed yet: the first one is written at the first stable release, with `release-prep` and `release-verify` (requests in the plan).
- `check-golden` is not in CI until a workflow step fetches `origin/main`.

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
