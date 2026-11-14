# R · M5 — Resource exhaustion

The fifth milestone of track R (`00-design.md` § M5, decision D-R-9). It runs the storage engine out of disk, memory and file descriptors, for real, and checks that each one ends in a clean refusal and never in a panic, an OOM kill, a wedged writer or a wrong result:
- **Disk.** A small filesystem fills during WAL writes, flush, compaction, query spill, read-cache fill and `put_media`. Each case checks the refusal that spec 03 §10 and D-A1-9 promise (`DiskFull`, `WalFailed`, a deferred or failed job, a query failing as too large), that a failed write leaves no temp file behind, and that once space is freed the storage recovers (after a restart for a WAL failure) and the M1 model's reads still match.
- **Spill cap.** Query spill files are not checked against `storage.min_free_bytes` today (`docs/research/storage/benchmarks.md` § Findings (a)). M5 caps them through DataFusion's disk manager, with a spec 03 §6 change.
- **Memory.** Runs under a cgroup memory limit at a documented minimum end in `503`/`422` refusals, never an OOM kill. M5 measures that minimum for the default config and for a small-machine profile.
- **File descriptors.** Runs under a low `RLIMIT_NOFILE` give errors, never a panic or a wedged writer. M5 measures the descriptors the engine needs.

Sub-phases R5.1–R5.8, one commit each. R5.8 can produce several commits, one per bug found.

**Decided by the user for this plan:**
- **Q1, memory:** document a minimum and a small-machine profile, and warn at open when the budgets exceed the detected limit. No auto-sizing (R5.6).
- **Q2, descriptors:** `llmobs serve` raises its soft `RLIMIT_NOFILE` to the hard limit. That is server code: R5.8 requests it from the coordinator.
- **Q3, WAL rotation:** a failed rotation is not final. It changes spec 03 §1, so R5.8 stops and reports it with the proposed decision text written out there.
- **Q4, CI:** the disk-full, memory and descriptor runs are run by hand. The nightly workflow does not run them.

## What was checked while writing this plan

- **A small filesystem without root works here** (Fedora, kernel 7.2): `unshare -rm sh -c 'mount -t tmpfs -o size=64m tmpfs ./small && df ./small'` shows a 64 MiB tmpfs. Inside, `dd` stops at 64 MiB with `No space left on device`, a `write()` fails with errno 28, and `fdatasync` succeeds (a no-op on tmpfs). The process is uid 0 inside the namespace, mapped to the user outside. `mount -o remount,size=…` of that tmpfs **fails** in the namespace, so the tests free space by deleting a ballast file, not by growing the filesystem.
- **A memory limit without root works here**: `systemd-run --user --scope -p MemoryMax=64M -p MemorySwapMax=0 <cmd>` runs `<cmd>` in a transient scope, and a process allocating 200 MiB is killed (exit 137).
- **The query pool does not see a systemd memory limit today.** `memory::default_total_memory` calls sysinfo's `System::cgroup_limits()`, which reads `memory.max` and `memory.current` at the root of the cgroup mount (`/sys/fs/cgroup`, `sysinfo-0.39.6/src/unix/linux/cgroup.rs::limits_for_system`). In a container with its own cgroup namespace that root is the container's cgroup, so Docker and Kubernetes limits are seen. On a host, the root has no `memory.current`, so a systemd unit's or scope's `MemoryMax=` is ignored and the pool is sized from the machine's RAM. Spec 03 §6 says "of the cgroup limit". R5.6 fixes it. The sandbox this plan was written in also runs in a cgroup namespace whose `/proc/self/cgroup` path starts with `/../..`, which no reader can resolve: R5.6's memory runs handle that case.
- **DataFusion 55.2's disk manager has a size limit.** `datafusion-execution-55.2.0/src/disk_manager.rs`: `DiskManager::set_max_temp_directory_size(&self, u64)` takes effect immediately for every later spill write, and the default is 100 GiB. The check is in `FileSpillWriter::write`: when the running total of spill bytes would pass the limit it returns `io::Error::other("The used disk space during the spilling process has exceeded the allowable limit of …")`. `RuntimeEnvBuilder::from_runtime_env` shares the base runtime's `Arc<DiskManager>` with every query runtime (`lib.rs::Storage::query`), so one setting caps all queries together. `with_max_spill_merge_fan_in` (default 0, unlimited) bounds how many spill files one merge opens.
- **That spill error is classified as `Failed` (400), not `TooLarge` (422).** It reaches `query_error::classify` as `DataFusionError::ArrowError(ArrowError::IoError(_, io::Error))` (the IPC stream writer wraps it), which matches the `ArrowError` arm. A real `ENOSPC` during a spill is wrapped once more: `FileSpillWriter` turns it into `DataFusionError::IoError`, then `io::Error::other(DataFusionError)`. R5.2 fixes both.
- **A failed local object write leaves its temp file.** `objects.rs::put_local` returns on a `write_all` error without removing `.<name>.tmp-<uuid>`, and `publish_local` does the same on a `sync_all` or rename error. In `compaction/job.rs` (the slice loop of the job runner), a `writer.finish()` error returns through `?` without `discard_output`. On a full disk each flush retry, under a new name, leaves another partial file, so the space never comes back until a restart. R5.3–R5.4 confirm it and R5.5 fixes it.
- **Error mapping that matters here.** `object_store`'s `LocalFileSystem` (0.13.2) maps an `EMFILE` on open to `Error::UnableToOpenFile`, which `verify::is_store_error` treats as a store failure, not a suspect file: running out of descriptors cannot quarantine data. A WAL segment rotation that fails (`SegmentWriter::create`, which also opens the directory to fsync it) fails the writer, so `EMFILE` or inode exhaustion at a rotation is final (§ R5.3 D9, R5.7).
- `rustix` 1.1.6 (workspace pin, features `std`, `fs`) has `rustix::process::{getrlimit, setrlimit, Resource::Nofile, Rlimit}` behind its `process` feature. `prlimit` and `systemd-run` are installed here; `unshare` and `mount` are util-linux.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Builds on M1–M4.** M5 starts after M2 (the `src/fs.rs` seam) and M4 (`ChaosStore`, an S3 mode of the model harness). Functions are named here by their names at the time of writing. If M2 moved a call into `fs.rs` or renamed it, follow the call to its new place and say so in the report.
- **Scope.** Edit only:
  - `crates/storage/src/model_tests/resources/` (new) and `crates/storage/src/model_tests/mod.rs` (the `mod` line);
  - `crates/storage/src/model_tests/harness.rs` (additions in R5.1, R5.3 and R5.4, each landing with its first caller);
  - R5.2: `crates/storage/src/disk.rs`, `lib.rs` (open order, `build_runtime`, the compaction runtime, and its `tests` module, which holds `a_spill_past_the_cap_fails_as_too_large`), `query_error.rs`, the tests of `sql/run.rs` that test `classify`, the `Disk::open` callers in `read_cache.rs` tests;
  - R5.5: `crates/storage/src/objects.rs`, `crates/storage/src/compaction/job.rs`;
  - R5.6: `crates/storage/src/memory.rs`, `lib.rs` (`resolve_query_memory`'s log line, and the open-time budget warning in `open_inner`);
  - R5.7: `crates/storage/Cargo.toml` (`[dev-dependencies]`);
  - `justfile`;
  - `CHANGELOG.md` § `[Unreleased]` (R5.2, R5.5, R5.6: one line each, given in the sub-phase);
  - `docs/spec/03-storage.md`, one line of `docs/spec/12-operations.md` (R5.8), `docs/research/storage/hardening.md`, `docs/roadmap.md` (R5.8).

  R5.8 may fix bugs anywhere in `crates/storage/src/` (D-R-2). It stops and reports instead when a fix would change a file format, a public API or a spec rule. `docs/self-hosting/` belongs to track N: M5 does not edit it (§ R5.8 Requests).
- **Dependencies.** No new crate. R5.7 enables `rustix`'s `process` feature for tests only:

  ```toml
  [dev-dependencies]
  rustix = { workspace = true, features = ["process"] }
  ```

  The version stays the workspace pin (1.1.6). `object_store::memory::InMemory` (object_store 0.13.2) and `datafusion::execution::disk_manager::{DiskManager, DiskManagerBuilder, DiskManagerMode}` (DataFusion 55.2) are already reachable.
- **All test code lives under `model_tests/resources/`.** It is a child module of `model_tests`, so it uses the M1 items (`Harness`, `Model`, `P`, `Op`, `apply`, `settle`, `check_full`, `shape::*`), which are `pub(super)` in their modules, without widening their visibility. Private `#[cfg(test)]` methods of `Storage` (`disk()`, `hooks()`, `staged_paths()`, `catalog_files()`, `verify_pass()`) and its private fields (`runtime`) are visible there too, because it is a descendant of the crate root.
- **Ignored tests.** Every resource test is `#[ignore = "<reason>; run by just resources…"]`: it needs a namespace, a cgroup scope, a lowered limit, or minutes. The cheap tests of R5.2 are ordinary tests and run in `just test`. R5.7 adds one short descriptor run to `just test`, as `just test` already runs the crash parents one by one.
- **Dead code between sub-phases.** R5.1 adds items that later sub-phases call first. `model_tests/resources/mod.rs` starts with `#![allow(dead_code)] // removed in R5.8, once every item is used`. R5.8 removes it and deletes what is still unused.
- **Panics.** Test code may `unwrap`/`expect`, with a message that says what was expected. Production code changed here follows `AGENTS.md`: no `unwrap`, no panic on input from disk or network, errors through the crate's `thiserror` enums.
- **Logging and privacy.** New production logs carry sizes, counts and kinds, never paths with project ids, payload text or error messages that could hold them (the existing pattern in `disk.rs` and `gc.rs`). Test output prints counts and byte sizes only, never span text.
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - the sizes and counts chosen where the plan says to tune them, and the measured values;
  - the run time of each new test, debug or release;
  - anything left undone or unverified.

### Names used throughout

- **The filler project** `C` = `"018f6b4e-7c1a-7c1a-8c1a-0000000000c3"`. Its rows only take space. The model never reads it, so a failed append of filler rows that a restart replays anyway (an unacknowledged WAL group, audit item 3 of `00-design.md`) cannot break a model check. Model rows (projects A and B) are only appended while space is known to be free.
- **The small filesystem** is either a 64 MiB tmpfs mounted in a private user and mount namespace (`unshare -rm`), or the directory named by `LLMOBS_TEST_SMALL_FS`: a small filesystem the operator mounted, for example an ext4 loop device (R5.1). The case code receives its root path and never knows which.
- **Ballast**: a file of zeros under the small filesystem's root that leaves a chosen number of bytes free. Freeing space means removing or shrinking it.
- **Day `D`, `midnight`, `day`, `HOUR_NS`, the clock**: as in M1 (`model_tests/shape.rs`, `01-m1-model.md` § Names).

---

## R5.1 — Small filesystem, filler and harness options

Commit: `storage: add the small-filesystem harness of the resource tests (R5.1)`

### `model_tests/harness.rs` additions

The resource cases need a data directory on the small filesystem, an S3 stand-in that outlives a life, and more worker threads. (A reopen that may fail comes in R5.3, where it is first used: M1's R1.7 removed `model_tests`'s `allow(dead_code)`, so every item added to `harness.rs` must have a caller in the same commit.)

```rust
/// How a harness opens its lives. `Default`: system temp dir, local objects, 2 workers (M1).
#[derive(Default)]
pub(super) struct HarnessOptions {
    /// Parent of the data directory; `None` is the system temp dir.
    pub(super) root: Option<std::path::PathBuf>,
    /// Passed to `Storage::open_with_store` in every life; `None` opens local objects.
    pub(super) store: Option<Arc<dyn object_store::ObjectStore>>,
    /// Tokio worker threads per life; 0 means 2.
    pub(super) workers: usize,
}

impl Harness {
    /// `tempdir_in(root)`, `model_config(dir)` changed by `adjust`, opened.
    pub(super) fn with_options(options: HarnessOptions, adjust: impl FnOnce(&mut StorageConfig)) -> Self;
}
```

- `Harness` keeps `store` and `workers` next to `config`, and `open_life(config, store, workers)` uses them for every life. `Harness::new()` becomes `Self::with_options(HarnessOptions::default(), |_| {})`.
- `root: None` keeps M1's behaviour (the system temp dir). Every resource test sets `root` explicitly (the small filesystem, or a disk-backed directory under `target/` for R5.6 and R5.7): `/tmp` is tmpfs on many machines, this one included.
- If M4 already gave `Harness` a store (its S3 mode), reuse that field instead of adding a second one, and say so in the report.

### `model_tests/resources/mod.rs`

```rust
//! Track R M5: the storage under a full disk, a memory limit and few file
//! descriptors (docs/plans/r-hardening/05-m5-resources.md).
#![allow(dead_code)] // removed in R5.8, once every item is used

mod fill;
mod small_fs;
```

R5.3–R5.7 add `disk_full`, `memory` and `descriptors`. `model_tests/mod.rs` gets `mod resources;`.

### `model_tests/resources/small_fs.rs`

```rust
/// Bytes of the namespace tmpfs.
pub(super) const SMALL_FS_BYTES: u64 = 64 << 20;

/// Options of the namespace tmpfs.
#[derive(Debug, Clone, Copy)]
pub(super) struct SmallFs {
    pub(super) bytes: u64,
    /// `nr_inodes=`; `None` keeps tmpfs's default.
    pub(super) inodes: Option<u64>,
}

/// Runs `case` with the root of a small, empty filesystem.
///
/// - `LLMOBS_SMALL_FS_CHILD` set: this process is the namespace child; `case`
///   runs on that mount.
/// - `LLMOBS_TEST_SMALL_FS` set: `case` runs in a new directory under it.
/// - Neither: re-executes the test `name` inside `unshare -rm` over a fresh
///   tmpfs and asserts that it exited successfully.
pub(super) fn on_small_fs(name: &str, fs: SmallFs, case: impl FnOnce(&std::path::Path));

/// `f_bavail * f_frsize` of the filesystem holding `path` (rustix `statvfs`).
pub(super) fn free_bytes(path: &std::path::Path) -> u64;
/// `f_favail` of the filesystem holding `path`.
pub(super) fn free_inodes(path: &std::path::Path) -> u64;

/// A file of zeros that takes space until removed.
pub(super) struct Ballast { path: std::path::PathBuf }

impl Ballast {
    /// Writes zeros to `root/ballast` until `free_bytes(root) <= leave`:
    /// 1 MiB writes, then one write of the remainder, then `sync_all`. Panics
    /// when the filesystem already has less than `leave` free.
    pub(super) fn leave_free(root: &std::path::Path, leave: u64) -> Self;
    /// Grows or shrinks the ballast (`set_len` down, zeros up) to leave `leave` bytes free.
    pub(super) fn resize_to_leave(&mut self, leave: u64);
    /// Removes the file; the space is free at once.
    pub(super) fn remove(self);
}

/// Empty files under `root/inode-ballast/` until `free_inodes(root) <= leave`.
pub(super) struct InodeBallast { dir: std::path::PathBuf }
impl InodeBallast {
    pub(super) fn leave_free(root: &std::path::Path, leave: u64) -> Self;
    pub(super) fn remove(self);
}

/// Samples `free_bytes(root)` every 5 ms on a thread; `stop` returns the least value seen.
pub(super) struct FreeSampler { /* stop flag, join handle */ }
impl FreeSampler {
    pub(super) fn start(root: &std::path::Path) -> Self;
    pub(super) fn stop(self) -> u64;
}

/// `.tmp-` files left under `dir`, recursively (local objects, `cache/`, `tmp/compaction/`).
pub(super) fn temp_files(dir: &std::path::Path) -> Vec<std::path::PathBuf>;
```

Ballast writes real zeros (`write_all` of a zeroed buffer), never `set_len` upwards: a sparse file takes no space.

**The namespace child.** The parent creates `mount = tempfile::tempdir()` and runs:

```text
unshare -rm sh -c 'mount -t tmpfs -o "$LLMOBS_SMALL_FS_OPTIONS" tmpfs "$LLMOBS_SMALL_FS_CHILD" && exec "$0" "$@"' \
    <current_exe> --exact <name> --ignored --nocapture --test-threads=1
```

with `LLMOBS_SMALL_FS_CHILD=<mount>` and `LLMOBS_SMALL_FS_OPTIONS=size=<bytes>,mode=0700[,nr_inodes=<n>]`, stdout piped, stderr inherited. It polls `try_wait` every 20 ms (reading stdout on a helper thread so the pipe never fills), kills the child after 300 s, and asserts `status.success()`, as `crash_tests.rs::run_child` does.

**A typo must not pass.** libtest given an `--exact` name that matches nothing runs 0 tests and exits 0. So the parent also asserts that the child's stdout contains `test result: ok. 1 passed`, and panics with the name otherwise. Each case takes its name from one constant next to it, used both for the call and nowhere else:

```rust
const D1: &str = "model_tests::resources::disk_full::appends_stop_at_min_free_before_the_disk_fills";

#[test]
#[ignore = "needs a small filesystem; run by just resources"]
fn appends_stop_at_min_free_before_the_disk_fills() {
    on_small_fs(D1, SmallFs { bytes: SMALL_FS_BYTES, inodes: None }, |root| { … });
}
```

The memory and descriptor parents (R5.6, R5.7) are safe from the same mistake: they require the child's `LLMOBS_MEMORY` / `LLMOBS_FD` result line, which a child that ran no test never prints. The mount is private to the child's namespace: every step of a case, restarts and model checks included, runs in the child. Tmpfs pages come from RAM, so 64 MiB per case.

- `unshare` fails with "Operation not permitted" where unprivileged user namespaces are off (some distributions; Ubuntu 24.04's AppArmor default restricts them). The parent then panics with `"unshare -rm failed: enable unprivileged user namespaces, or set LLMOBS_TEST_SMALL_FS to a small mounted filesystem"`, plus the child's stderr.
- With `LLMOBS_TEST_SMALL_FS`: `case` runs on `tempfile::tempdir_in(root)` in the current process. All cases then share one filesystem, so they must run one at a time: `--test-threads=1`, which `just resources` passes. Running a case by hand with `cargo test` needs it too. `on_small_fs` panics when the filesystem has less than 48 MiB or more than 1 GiB free (ballast writes would be too slow). `SmallFs::inodes` is ignored there; D9 uses `InodeBallast` on both kinds.

**Why tmpfs is not the whole answer.** tmpfs reports `ENOSPC` at `write()`, never later: it has no delayed allocation and no writeback, `fsync` is a no-op, and it has no journal or metadata blocks to run out of. So it misses:
- `ENOSPC` or `EIO` reported by `fsync` after writeback, as on thin provisioning, NFS, or btrfs and XFS when metadata or speculative preallocation runs out. That path is a failed fsync, which M2's injected fsync errors (D-R-6) cover.
- Space taken by journal and metadata, and ext4's reserved blocks (`f_bavail` already excludes them).
- btrfs needing free space to delete a file.

`LLMOBS_TEST_SMALL_FS` over an ext4 (or XFS, btrfs) loop device runs the same cases with real allocation and journaling. The setup needs root once and is documented in `hardening.md` (R5.8):

```bash
truncate -s 128M /var/tmp/llmobs-small.img && mkfs.ext4 -q /var/tmp/llmobs-small.img
sudo mkdir -p /mnt/llmobs-small && sudo mount -o loop /var/tmp/llmobs-small.img /mnt/llmobs-small
sudo chown "$USER" /mnt/llmobs-small
LLMOBS_TEST_SMALL_FS=/mnt/llmobs-small just resources
```

### `model_tests/resources/fill.rs`

```rust
pub(super) const FILLER_PROJECT: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000c3";
pub(super) fn filler() -> ProjectId;

/// What a filler span's `input` holds.
#[derive(Debug, Clone, Copy)]
pub(super) enum Text {
    /// Words from a 512-word vocabulary, chosen by a xorshift64 seeded with
    /// (batch, span): compresses like the benchmarks' payloads.
    Words,
    /// A 16-byte prefix unique per (batch, span), then one 64-byte pattern
    /// repeated: tiny once compressed, full size in Arrow (and in spill files).
    Repeated,
}

/// One trace `1000 + batch` of project C: `spans` spans (1..=200) with an
/// `input` of `input_bytes` bytes. Span `s` starts at
/// `midnight(day(day_offset)) + 9 h + s seconds`; every row has `ingest_time = ingest`.
pub(super) fn filler_batch(batch: u16, spans: u8, input_bytes: usize, text: Text, day_offset: u8, ingest: i64) -> RecordBatch;

/// Rows of project C readable now: a count over the raw `spans` provider of
/// C's `ProjectQuery`, as `harness.rs::visible_span_rows` counts project A.
pub(super) fn filler_rows(h: &Harness) -> u64;

/// Appends filler batches (`next(i)` builds batch `i`) until `stop` returns
/// true for an outcome or `max_batches` were sent, sleeping `pace` between
/// appends. Returns every outcome in order.
pub(super) fn append_filler(
    h: &Harness,
    next: impl FnMut(u16) -> RecordBatch,
    pace: std::time::Duration,
    stop: impl FnMut(&Result<AppendReceipt, AppendError>) -> bool,
    max_batches: u16,
) -> Vec<Result<AppendReceipt, AppendError>>;
```

`filler_batch` starts from `test_support::span_row(1000 + batch, s, start, ingest)` and sets `project_id`, `input` and `name = "filler"`. It asserts `batch < 60_000` (the trace number must fit `span_row`'s `u16`). Long-running loops (R5.6, R5.7) pass `counter % 60_000`: a reused trace number is a re-send of the same spans with a newer ingest time, which is harmless for filler. Those loops also stop at a total-bytes cap given in their sub-phase, so the disk use stays bounded. The vocabulary is generated once from the same xorshift (lowercase words of 3–10 letters); no external corpus. Each case passes the model's clock (after an `op_tick`-sized step of its own) as `ingest`, so ingest times stay increasing.

### `model_tests/resources/mod.rs`: shared steps

```rust
/// A/B ops every case applies before its pressure phase.
fn seed_ops() -> Vec<Op>;
/// A/B ops every case applies after recovery.
fn after_ops() -> Vec<Op>;
/// Applies `ops` with M1's `apply`, checking after each one exactly as
/// M1's `run_case` does (below).
fn model_steps(h: &mut Harness, model: &mut Model, clock: &mut i64, ops: &[Op]);
/// M1's `settle`, then `check_full`: everything flushed, applied, compacted and checked physically.
fn settle_and_check(h: &mut Harness, model: &mut Model, clock: &mut i64);
```

- `seed_ops()`: `Append { A, 0, 6, false }`, `Append { B, 0, 6, false }`, `Append { A, 1, 4, true }`, `Score { A, 0, 0 }`, `Append { A, 5, 6, false }`, `Restart`.
- `after_ops()`: `Append { A, 2, 4, false }`, `Resend { A, 0, 1 }`, `Score { B, 13, 1 }`, `Restart`.
- **Checks in `model_steps`**, copying `run_case`'s loop: after `Restart`, `Crash` and `Settle`, `check_full(h, model, index)`; after an op with a project, `check_quick(h, model, project, index)`; after an op without one, `check_quick` on both projects. `index` is the op's position in the `ops` slice, plus 100 for `after_ops` so failure messages tell the two lists apart.
- **`settle_and_check`** calls `check_full(h, model, 1_000)` after `settle`.
- **Elsewhere in a case**, a bare `check_full` call uses index `500`.
- The ops never fail while space is free (M1's `apply` asserts `Ok`).
- **The clock.** `apply` advances `clock`. A case that appends filler or calls `compact` uses the same `clock`: it adds 1 µs before each filler batch and passes `*clock` as `ingest`. It passes `*clock` (always past the last ingest) to `compact`, unless the case says otherwise.

### Test (R5.1)

`model_tests::resources::small_fs::small_fs_fills_and_frees` (`#[ignore = "needs a small filesystem; run by just resources"]`), through `const SMOKE: &str = "model_tests::resources::small_fs::small_fs_fills_and_frees";` and `on_small_fs(SMOKE, SmallFs { bytes: SMALL_FS_BYTES, inodes: None }, |root| …)`:
1. `free_bytes(root)` is between 48 MiB and 1 GiB.
2. `Ballast::leave_free(root, 4 << 20)`: free is at most 4 MiB.
3. Writing 8 MiB to a new file fails with `ErrorKind::StorageFull`.
4. Remove that file and the ballast: free is back within 1 MiB of step 1.
5. A `Harness::with_options` on `root`, `model_steps(seed_ops())`, then `settle_and_check`: the M1 checks pass on the small filesystem.

### `justfile`

After `model`:

```just
# Resource exhaustion (track R M5, D-R-9): a full disk and few file descriptors.
# Needs unprivileged user namespaces, or LLMOBS_TEST_SMALL_FS (a small mounted filesystem).
resources:
    cargo test --workspace --all-features -- --ignored --test-threads=1 model_tests::resources::small_fs:: model_tests::resources::disk_full:: model_tests::resources::descriptors::
```

`--workspace --all-features` reuses the build of `just test` (the comment above the crash tests in `test:` says why). libtest accepts several filters. `disk_full::` and `descriptors::` match nothing until R5.3 and R5.7.

### Commands

```bash
just resources
LLMOBS_TEST_SMALL_FS=<a small mount> just resources   # when one is available
```

Expected: 1 passed. Report whether `unshare` worked here, and the run time.

---

## R5.2 — Spill capped against free disk

Commit: `storage: cap query spill at the free disk above min_free_bytes (R5.2)`

### What it does (spec 03 §6, D-R-9)

Spill files may use the free disk above `storage.min_free_bytes`, and no more. At every measurement the disk monitor sets DataFusion's limit to:

```text
cap = spilled_now + free.saturating_sub(min_free)
```

`spilled_now` is `DiskManager::used_disk_space()`. `free` is measured with those spill files already on disk, so the formula lets spill grow by exactly the free space above the threshold. Spill writes after the measurement raise `spilled_now` and lower `free` by the same amount, so the cap stays right until the next measurement. Only other writers (WAL, flush, compaction) can make it stale, by at most one second of their writes; `min_free_bytes` has to cover that anyway (R5.8 § Resource needs). Under the threshold the cap is `spilled_now`: running queries keep their files, and any new spill write fails. The `DiskManager` is shared by every query's runtime (`RuntimeEnvBuilder::from_runtime_env`), so the cap covers all queries together.

A query whose spill passes the cap, or that gets `ENOSPC` or `EDQUOT` while spilling, fails as `RunError::TooLarge` (422 `query_too_large`).

No config key is added: the bound is the one the operator already sets, `storage.min_free_bytes`.

### Changes

**`disk.rs`**
- `Disk` gets a field `spill: Arc<DiskManager>` (`datafusion::execution::disk_manager::DiskManager`). `Disk::open(paths, min_free, spill)` takes it, and applies the cap once after its first measurement.
- `Disk::refresh` stores the measurement, then calls `self.apply_spill_cap(free)`:

  ```rust
  /// Sets the spill limit to the bytes spilled now plus the free disk above `min_free`.
  fn apply_spill_cap(&self, free: u64) {
      let cap = self.spill.used_disk_space().saturating_add(free.saturating_sub(self.min_free));
      if let Err(error) = self.spill.set_max_temp_directory_size(cap) { /* warn once, below */ }
  }
  ```

  `set_max_temp_directory_size` fails only for a disabled disk manager, which the query runtime never is. Log `tracing::warn!(%error, "setting the query spill limit failed")` the first time only: add a `spill_cap_warned: AtomicBool`.
- `set_free_for_test` already calls `refresh`, so tests see the cap move at once.
- `#[cfg(test)] pub(crate) fn test_spill(dir: &Path) -> Arc<DiskManager>`: `Arc::new(DiskManagerBuilder::default().with_mode(DiskManagerMode::Directories(vec![dir.to_path_buf()])).build().expect("test disk manager"))` (`build` returns a `Result`), for the unit tests that build a `Disk` (`disk.rs` and `read_cache.rs` tests). Update those callers.

**`lib.rs::Storage::open_inner`**
- Move the spill-file cleanup (`remove_spill_files`) and `build_runtime` before `disk::Disk::open`. Neither depends on the object store or the disk. Then `Disk::open(disk_paths, config.min_free_bytes.0, Arc::clone(&runtime.disk_manager))`. `objects.register(&runtime)` stays where it is, after `objects` is final (after the read cache is attached).
- The compaction runtime (`RuntimeEnvBuilder::new()…` in the `compactor` block) has no temp path today, so any spill would go to the OS temp directory, uncapped. Its merge (`SortPreservingMergeExec`) never spills. Make that explicit: `.with_disk_manager_builder(DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled))`. Run the compaction tests: if one now fails with "DiskManager is disabled", something in compaction spills. Stop and report.

**`query_error.rs::classify`**
- Before the `ArrowError` arm, check `spill_exhausted(root)`; true gives `RunError::TooLarge`.

  ```rust
  /// DataFusion's spill limit text (`datafusion-execution` 55.2, `FileSpillWriter::write`).
  /// `a_spill_past_the_cap_fails_as_too_large` breaks if an upgrade changes it.
  const SPILL_LIMIT_TEXT: &str = "exceeded the allowable limit";

  /// The query ran out of spill space: DataFusion's limit, ENOSPC or EDQUOT,
  /// found in the I/O errors under `error`.
  fn spill_exhausted(error: &DataFusionError) -> bool;
  /// `kind()` is `StorageFull` or `QuotaExceeded`, or the text has
  /// `SPILL_LIMIT_TEXT`, or `get_ref()` holds a `DataFusionError` or an
  /// `io::Error` that does (recursively, depth at most 8).
  fn io_exhausted(error: &std::io::Error, depth: u8) -> bool;
  ```

  `spill_exhausted` looks at `DataFusionError::IoError(e)`, at `DataFusionError::ArrowError(a, _)` when `*a` is `ArrowError::IoError(_, e)`, and at `ArrowError::ExternalError(b)` / `DataFusionError::External(b)` whose `b` downcasts to `DataFusionError` or `io::Error`. `io::Error::source()` skips the custom payload, so use `get_ref()`. If the first real run (test 3 below) fails differently, print `{error:?}`, extend the walk to the shape it shows, and report it.
- `RunError::TooLarge`'s doc says "more memory than its pool allows, or more spill space than the disk has above `storage.min_free_bytes`". Change its `#[error]` text to `"the query needed more memory or temporary disk space than allowed"`. First `grep -rn "needed more memory than allowed" crates/ fixtures/api/ docs/spec/09-api.md` (check the file name of spec 09 with `ls docs/spec/`). If a test outside `crates/storage`, a fixture or spec 09 quotes the old text, keep the old text and report it: those files are outside M5's scope. The OpenAPI descriptions in `crates/server/openapi.json` paraphrase it ("needed more memory than allowed"); they are the server's, and the report requests their update.

### Spec (in this commit)

`docs/spec/03-storage.md` §6 *Resource limits*:
- In **Memory**, "(default 50% of RAM or of the cgroup limit, at most 8 GiB)" becomes "(default 50% of RAM or of the memory limit of the process's cgroup (its systemd unit or container), at most 8 GiB)". R5.6 makes the code match.
- New bullet after **Admission**:

  > - **Spill:** spill files in `tmp/query/` may use the free disk above `storage.min_free_bytes` and no more, all queries together. At every measurement (§10) the disk monitor sets the limit to the bytes spilled now plus `free − min_free_bytes`. Under the threshold no query spills further. A query whose spill would pass the limit, or that gets `ENOSPC` while spilling, fails as too large (422).

§10, first row (free disk under `storage.min_free_bytes`): after "flushes don't reserve (D-A1-9)", add "; query spill stops at the threshold (§6)".

### Tests (in `just test`)

1. `disk::tests::the_spill_cap_is_the_free_disk_above_min_free`: `Disk::open([dir], 100, test_spill(dir))`. `set_free_for_test(Some(1_000))`: `max_temp_directory_size() == 900`. `Some(50)`: `0`. `Some(100)`: `0`.
2. Next to `classify`'s existing tests in `sql/run.rs`:
   - `a_spill_limit_error_is_too_large`: `DataFusionError::ArrowError(Box::new(ArrowError::IoError("x".into(), io::Error::other("The used disk space during the spilling process has exceeded the allowable limit of 1.0 B."))), None)` classifies as `TooLarge`.
   - `enospc_under_a_spill_is_too_large`: `ArrowError::IoError(_, io::Error::other(DataFusionError::IoError(io::Error::from(io::ErrorKind::StorageFull))))`, wrapped as above, gives `TooLarge`.
   - `another_arrow_io_error_still_fails`: `ArrowError::IoError(_, io::Error::other("broken pipe"))` stays `Failed`.
3. `lib.rs` tests, `a_spill_past_the_cap_fails_as_too_large`:
   - Config `config_in(dir)` with `min_free_bytes = 1 MiB`, `query.target_partitions = 1`, `query.max_memory = query.max_memory_total = 32 MiB`, `query.initial_memory = 16 MiB`.
   - Append 40,000 spans of one trace with a 1 KiB `input` each (distinct span ids, as `a_query_that_cannot_spill_fails_as_too_large` builds them), then `shutdown` (flush) and reopen.
   - SQL through `QueryContext::sql_json`: `SELECT count(*) AS n FROM (SELECT row_number() OVER (ORDER BY input, span_id) AS r FROM spans) WHERE r > 0`, limit 1, timeout 30 s. The window sorts every row, and the outer count returns one row: there is no `TopK` that would avoid the sort.
   - Run 1, real free disk: `Ok`. Run 2, after `storage.disk().set_free_for_test(Some(1 << 20))` (cap = 0): `Err(RunError::TooLarge)`. If run 2 is `Ok`, the query did not spill: raise the span count until it does, and report the count. Run 3, `set_free_for_test(None)`: `Ok` again.
   - After run 2, the failed query's spill files go away. They are dropped asynchronously, as the query's tasks wind down, so poll `storage.runtime.disk_manager.used_disk_space() == 0` every 50 ms with a 10 s deadline, as the cancellation test in `sql/run.rs` polls `memory_pool.reserved()`.

### CHANGELOG

`CHANGELOG.md` § `[Unreleased]` → `Fixed`: "A query that would spill past the free disk above `storage.min_free_bytes` now fails as too large (422) instead of filling the disk."

### Commands

```bash
cargo test -p llmobs-storage --all-features spill
cargo test -p llmobs-storage --all-features too_large
just check
```

---

## R5.3 — Full disk: WAL, media, inodes

Commit: `storage: test the WAL and media on a full disk (R5.3)`

### `model_tests/harness.rs`: a reopen that may fail

D2 and D9 are the first callers, so these land here:

```rust
impl Harness {
    /// Drop without shutdown, then try to open a new life. On `Err` there is
    /// no life until `open_again`.
    pub(super) fn crash_then_try_open(&mut self) -> Result<(), crate::error::OpenError>;
    /// Opens a life when there is none (after a failed `crash_then_try_open`).
    pub(super) fn open_again(&mut self);
}
```

- `crash_then_try_open`: take the life, drop the storage, `runtime.shutdown_timeout(10 s)`, then build a runtime (same `workers`) and `block_on(Storage::open…)` (or `open_with_store` with the harness's store). On `Err`, shut that runtime down, leave `life = None` and return the error. `storage()`, `block_on`, `restart` and `crash` keep `expect("a life")`.
- `open_again` asserts `life.is_none()` and opens one with `open_life`.
- `restart` expects a clean shutdown. After a `WalFailed` the shutdown reports the writer failure, so the cases use `crash` or `crash_then_try_open` there.

### The cases

Each case is a `#[test] #[ignore = "needs a small filesystem; run by just resources"]` in `model_tests/resources/disk_full.rs`, whose body is `on_small_fs(<its NAME constant>, SmallFs { bytes: SMALL_FS_BYTES, inodes: None }, |root| { … })`, with the constant declared as in R5.1. Every harness uses `HarnessOptions { root: Some(root.into()), ..Default::default() }` and, besides the case's own changes, `wal.segment_bytes = 1 MiB` (the minimum). Call `h.storage().disk().refresh()` after every ballast change, so the storage sees the new free space without waiting for the monitor.

### D1 `appends_stop_at_min_free_before_the_disk_fills`

`min_free_bytes` keeps the WAL from hitting `ENOSPC` when it covers what can be written in one monitor interval.
- Config: `min_free_bytes = 16 MiB`. The memtable holds everything (`flush.max_age` 1 h, `flush.max_bytes` 256 MiB): only the WAL takes space.
- `model_steps(seed_ops())`.
- `Ballast::leave_free(root, 32 MiB)`.
- `append_filler` with `Text::Words`, 64 spans × 16 KiB per batch, `pace` 100 ms, until the first `Err`. That is about 10 MiB/s of Arrow, below `min_free` per second.
- Assert:
  - the first error is `AppendError::DiskFull`;
  - no `WalFailed` was ever returned, and `health().spans_wal_failed` is false;
  - `free_bytes(root)` is still above 0.
- Remove the ballast and `refresh`. One more filler append is `Ok`.
- `model_steps(after_ops())`, `settle_and_check`. `filler_rows` equals the rows of every `Ok` filler append.

### D2 `a_full_disk_fails_the_wal_and_a_restart_recovers`

With `min_free_bytes = 0` nothing refuses appends, and the WAL meets `ENOSPC` itself (spec 03 §1: write errors are final).
- Config: `min_free_bytes = 0`; flushes as in D1.
- `model_steps(seed_ops())`. `Ballast::leave_free(root, 8 MiB)`.
- `append_filler`, no pace, until the first `Err`.
- Assert:
  - that error is `WalFailed`, and every later append is `WalFailed`;
  - `health().spans_wal_failed` is true;
  - an append after `ballast.resize_to_leave(32 MiB)` and `refresh` is still `WalFailed` (final until restart).
- `ballast.resize_to_leave(64 KiB)` (a few pages for SQLite's `-shm` and the `LOCK` file, which already exist), then `h.crash_then_try_open()` while the disk is nearly full. `Ok` is expected: opening writes nothing that needs space (replay fills the memtable; `catalog.db` is read). If it is `Err`, record the variant. Any `OpenError` except `Config` and `Locked` is acceptable (a full disk is not a config error, and `Locked` would mean a task of the old life survived); then remove the ballast and `open_again`. A panic fails the case.
- With a life: `check_full`. The acknowledged A/B rows come back from WAL replay. `filler_rows` is between the rows of the `Ok` appends and that plus the rows of the one failed batch: an unacknowledged group may be replayed (audit item 3, accepted).
- Remove the ballast. `h.restart()` (shutdown flushes): `Ok`. `model_steps(after_ops())`, `settle_and_check`.

### D8 `put_media_on_a_full_disk_fails_and_leaves_nothing`

Spec 03 §9 and §10. Two harnesses in turn, because a harness's config is fixed.
- **Refusal.** Harness 1 with `min_free_bytes = 2 MiB`. `model_steps(seed_ops())`. `Ballast::leave_free(root, 1 MiB)`, `refresh`. `put_media(PROJECT_A, a 512 KiB blob)` is `Err(MediaError::DiskFull)`. Remove the ballast, drop harness 1 (its directory goes with it).
- **ENOSPC.** Harness 2 with `min_free_bytes = 0`. `model_steps(seed_ops())`. `Ballast::leave_free(root, 1 MiB)`. `put_media` of a 3 MiB blob:
  - is `Err(MediaError::Object(ObjectError::Io { source, .. }))` with `source.kind() == ErrorKind::StorageFull`;
  - `get_media` of its hash is `Err(MediaError::NotFound)`;
  - `temp_files(&data_dir.join("objects"))` is empty (expected to fail until R5.5; record it);
  - `staged_paths()` names `media/<project>/<sha>`; `collect_staged(Duration::ZERO)` removes it, and the path is no longer staged.
- Remove the ballast, `refresh`. The same `put_media` is `Ok` and `get_media` returns the same bytes.
- `model_steps(after_ops())`, `settle_and_check`.

### D9 `running_out_of_inodes_is_an_error_not_a_panic`

Bytes are free but no file can be created.
- `SmallFs { bytes: SMALL_FS_BYTES, inodes: Some(4096) }`. Config `min_free_bytes = 0`, `flush.max_bytes = 1 MiB` (a flush every few batches).
- `model_steps(seed_ops())`. `InodeBallast::leave_free(root, 4)`.
- `append_filler` (pace 20 ms, 32 spans × 4 KiB) for at most 400 batches or until the first `Err`. Expected from today's code:
  - a WAL rotation fails to create its segment, so the writer fails (`WalFailed`);
  - flushes fail to create their objects and retry.

  Record which comes first. Either way: no panic, `free_bytes(root)` is above 0, and `temp_files` of `objects/` is empty.
- Remove the inode ballast. `crash_then_try_open()` is `Ok`. `check_full`; `filler_rows` within the bounds of D2. `h.restart()`, `model_steps(after_ops())`, `settle_and_check`.
- A `WalFailed` here is a finding for R5.8 (§ R5.8, the WAL rotation item), not a failure of the case: assert the first error is `WalFailed` or that there was none, and print which.

### Commands

```bash
just resources
```

Expected: D1, D2 and D9 pass. D8 (`put_media_on_a_full_disk_fails_and_leaves_nothing`) fails only on its temp-file assertion, if R5.5's bug is confirmed; record the leftover file count. These are ignored tests, so `just check` still passes. Say so in the commit body: "Expected to fail until R5.5: disk_full::put_media_on_a_full_disk_fails_and_leaves_nothing (temp file left)", and name it in the report. Any other failure is a finding for R5.8: record the case, the assertion and the output, and leave the fix to R5.8.

---

## R5.4 — Full disk: flush, compaction, spill, read cache

Commit: `storage: test flush, compaction, spill and the read cache on a full disk (R5.4)`

Same file and conventions as R5.3. Sizes below are targets: check the actual file sizes with `catalog_files(Table::Spans)` and the spill volume as each case says, adjust the batch counts, and report the values. Every case starts with `model_steps(seed_ops())` before its filler and ends with `model_steps(after_ops())` and `settle_and_check` once space is free.

### D3 `a_flush_on_a_full_disk_retries_and_leaves_no_temp_files`

Spec 03 §2 step 4 and §10: a failed write keeps `frozen` and retries under a new name, and backpressure applies past the hard limit.
- Config: `min_free_bytes = 0`, `flush.max_bytes = 4 MiB`, `flush.max_age = 1 h`, `memtable.hard_limit = 24 MiB`.
- `h.storage().hooks().hold(HookPoint::FlushAfterStage)`. Append filler (`Text::Words`, 64 × 16 KiB) until `hooks().waiting_at(HookPoint::FlushAfterStage) == 1`, plus 2 more batches: a flush is staged and paused, with about 4 MiB frozen.
- Stop appending. `Ballast::leave_free(root, 256 KiB)`, so the flush's Parquet output cannot fit. `hooks().release(HookPoint::FlushAfterStage)`.
- Wait 4 s (at least two attempts, at 1 s and 2 s backoff). Assert:
  - `catalog_files(Table::Spans)` has no file of project C;
  - `temp_files(&objects)` is empty (expected to fail until R5.5);
  - `free_bytes(root)` is at least 192 KiB: failed attempts gave their space back (expected to fail until R5.5);
  - `health().spans_wal_failed` is false;
  - `check_full`: the A/B rows are still read from the memtable.
- Remove the ballast. Wait up to 70 s (one `MAX_BACKOFF` and slack) for a spans file of project C in `catalog_files`, polling every 200 ms. `filler_rows` equals the rows of the `Ok` appends.

### D4 `a_compaction_that_does_not_fit_is_deferred_until_it_does`

Spec 03 §5 *Resources* and D-A1-9, with real `statvfs` numbers.
- Config: `min_free_bytes = 0`.
- Two L0 spans files of project C in day `D`, about 4 MiB each: append filler, `h.restart()`, append filler, `h.restart()`. `inputs` = the sum of their `size`.
- `Ballast::leave_free(root, inputs / 2)`, `refresh`. `compact(clock)`: `report.jobs == 0`, `report.failed >= 1`, the catalog's spans files are unchanged, `temp_files` is empty.
- `ballast.resize_to_leave(inputs * 3)`, `refresh`. `compact(clock)`: `report.jobs >= 1`, and project C's day `D` has one spans file.

### D5 `a_compaction_that_meets_enospc_fails_cleanly_and_runs_later`

The reservation passed, then another writer took the space.
- Setup as D4, with free disk ample for the reservation.
- `hooks().hold(HookPoint::CompactionAfterStage)`. In `std::thread::scope`, one thread runs `h.block_on(h.storage().compact(clock))` (a multi-thread runtime accepts `block_on` from two threads at once). The main thread waits for `waiting_at(CompactionAfterStage) == 1`, then `Ballast::leave_free(root, 128 KiB)`, then releases the hook and joins.
- Assert:
  - the pass returned `Ok(report)` with `report.jobs == 0 && report.failed >= 1`;
  - the catalog's files are unchanged, and `check_full` holds;
  - `temp_files(&objects)` is empty, and `free_bytes(root) >= 64 KiB` (expected to fail until R5.5 if the merge failed in `writer.finish()`);
  - `staged_paths()` holds the job's output names; `collect_staged(Duration::ZERO)` empties it.
- Remove the ballast. `compact(clock)` commits the job.

### D6 `a_spilling_query_stops_at_min_free`

R5.2's cap on a real filesystem.
- Config: `min_free_bytes = 16 MiB`, `memtable.hard_limit = 512 MiB`, `flush.max_bytes = 64 MiB`, `query.target_partitions = 1`, `query.max_memory = query.max_memory_total = 32 MiB`, `query.initial_memory = 16 MiB`.
- **Sizing.** The filesystem is 64 MiB, `min_free` takes 16 MiB, and the WAL, catalog and Parquet files take a few more, so run 1 has about 40 MiB of room to spill. Aim for a spill of about 30 MiB: 60–70 MiB of Arrow sorted with a 32 MiB pool spills roughly the part that does not fit. Filler `Text::Repeated`, 200 spans × 2 KiB per batch, about 160 batches (about 64 MiB of `input`), in chunks of 40 batches with a `h.restart()` after each chunk: the WAL stays small, the Parquet files are small (repeated text), the Arrow data is large.
- The query is R5.2's window count, on project C through `QueryContext::sql_json`.
- **Room left.** The WAL is not where the room goes as long as each chunk ends with a restart (the flush truncates it), but the bytes written between two restarts are WAL bytes on the small filesystem. Check how `wal/record.rs` encodes the payload (spec 03 §1 says LZ4-frame bodies; if the code writes it uncompressed, a 40-batch chunk is about 16 MiB of WAL). If the filler count grows, shrink the chunks so that a chunk's WAL plus the files never take the 16 MiB above `min_free` away from the spill: check `free_bytes(root)` after the last restart, before run 1, and report it.
- **Run 1, room to spill.** No ballast. A thread polls `h.storage().runtime.disk_manager.used_disk_space()` every 5 ms for its peak. The query is `Ok` with `n` = the filler row count, and `peak > 0`.
  - If run 1 fails as `TooLarge`, the spill did not fit in the room left: **shrink** the filler by a quarter and retry, and report the count.
  - If `peak` is 0, the query did not spill: grow the filler by a quarter, staying within the room, or lower `query.max_memory` to 24 MiB.
- **Run 2, too little room.** `Ballast::leave_free(root, 16 MiB + peak / 2)`, `refresh`. `FreeSampler::start(root)`. The query is `Err(RunError::TooLarge)`. The sampler's minimum is at least 15 MiB: the spill never took the space kept for ingestion. Afterwards, poll `used_disk_space() == 0` every 50 ms with a 10 s deadline (spill files are dropped asynchronously, as in R5.2's test 3). Then `tmp/query/` holds only empty `datafusion-*` directories.
- A filler append right after run 2 is `Ok` (not `DiskFull`, not `WalFailed`).
- Remove the ballast, `refresh`. Run 1 again: `Ok`.

### D7 `the_read_cache_never_fills_the_disk`

Spec 03 §6 *S3 read cache*: blocks are written only while free disk is at least `2 × min_free_bytes`, and given back under `min_free_bytes`.
The pressure phase must start with an **empty cache and plenty of free space**: flushes write through into the cache, so a cache that is on while the data is seeded would already be full, and every later read would be a hit. So the data is seeded with the cache off, and read in a second life with the cache on.

**Harness hook (R5.4, D7 is its caller):**

```rust
impl Harness {
    /// Clean shutdown, `adjust` applied to the stored config, then a new life with it.
    pub(super) fn restart_with(&mut self, adjust: impl FnOnce(&mut StorageConfig));
}
```

It does what `restart` does, but changes `self.config` between the shutdown and the next `open_life`. Later restarts keep the changed config.

**Steps:**
- **Seed, cache off.** `HarnessOptions { store: Some(Arc::new(object_store::memory::InMemory::new())), .. }`. Config: `min_free_bytes = 8 MiB`, `query.read_cache_bytes = Some(ByteSize::b(0))` (0 turns the cache off, spec 03 §6, so nothing is written under `cache/`), `flush.max_bytes = 8 MiB`.
  - `model_steps(seed_ops())`.
  - Filler `Text::Words`, 64 × 16 KiB, in chunks of 8 batches with a `h.restart()` after each, until project C's spans files in the store total at least 40 MiB. The WAL is truncated after each flush, so the small filesystem holds only the WAL and the catalog.
- **Second life, cache on.** `h.restart_with(|c| c.query.read_cache_bytes = Some(ByteSize::mib(48)))`. 48 MiB is more than the filesystem can give, so free disk is the binding limit. The cache opens empty: assert `read_cache_stats()` is `Some` with `blocks == 0`. Assert `free_bytes(root) > 24 MiB`; if not, seed less and report.
- `Ballast::leave_free(root, 24 MiB)`, `refresh`.
- With a `FreeSampler` running, 5 times: `SELECT count(*) AS n, sum(length(input)) AS b FROM spans` on project C. Each run returns the filler's row count and byte sum.
- Assert:
  - after the first query, `read_cache_stats().misses > 0` and `blocks > 0`: the reads went to the store and the cache wrote blocks;
  - the sampler's minimum is at least `min_free_bytes − 1 MiB` (7 MiB). The cache must stop writing at `2 × min_free`, so dropping below `min_free` means it wrote on a stale measurement (`ReadCache::writable` reads the last one, up to 1 s old). That is a finding for R5.8, which has a fix ready;
  - within 5 s after the queries (the monitor sheds blocks every second), `free_bytes(root) >= 8 MiB`;
  - `temp_files(&cache_dir)` is empty;
  - an A append (`Op::Append`) after the queries succeeds.
- Remove the ballast. The model's reads at the end go through the cache: they must match.

### Commands

```bash
just resources
```

Expected: D4, D6 and D7 pass. D3 (`a_flush_on_a_full_disk_retries_and_leaves_no_temp_files`) and D5 (`a_compaction_that_meets_enospc_fails_cleanly_and_runs_later`) may fail only on their temp-file or free-space assertions, until R5.5. Name them in the commit body ("Expected to fail until R5.5: …") and in the report. D7 failing on its sampler assertion is a finding for R5.8; name it the same way. Record every failure as in R5.3.

---

## R5.5 — A failed local write removes its temp file

Commit: `storage: remove the temp file of a failed local object write (R5.5)`

**The bug** (found by reading; R5.3 D8 and R5.4 D3/D5 confirm it). A local object write that fails leaves its temp file until the next start. On a full disk, each flush retry writes a new partial temp file under a new name, so freed space is eaten again and the disk stays full until a restart.

**The fix:**
- Add `fn discard_temp(path: &Path)` next to `remove_temp_path` in `objects.rs`: remove the file, ignore `NotFound`, and on another error log `tracing::warn!("removing a failed object temp file failed")`, with no path.
- `objects.rs::put_local`: when `write_all` fails, `discard_temp(&temp_path)`, then return the write error.
- `objects.rs::publish_local`: when `sync_all` fails, and when `fs::rename` fails, `discard_temp(temp_path)` before returning. Leave the `ObjectDirSync` fault path as it is: it renames the file back on purpose, for the crash tests. After a real `sync_dir` error the bytes are already under the final name, and nothing is removed (as today). If M2 changed what a failed fsync does (D-R-6: the job fails and the store reports unhealthy), keep that, and remove the temp file as well.
- `compaction/job.rs`, the slice loop: when the `finished` block (`writer.finish()`, the footer read) returns `Err`, call `self.objects.discard_output(target).await` before returning. `target` is still owned there; only `target.temp()` was copied. When `publish_output` fails in local mode, `publish_local` now removes the temp file itself; in remote mode `publish_output` already removes its spill file.

**Regression tests.** R5.3 D8 and R5.4 D3/D5 are the named regression tests: they now pass in full. If M2's `src/fs.rs` can inject a write error (look for an API that makes the next write of a path fail), also add `objects::tests::a_failed_local_write_leaves_no_temp_file`, in `just test`: inject the error into a `put_durable`, assert `Err`, and assert that no `.tmp-` file is left under the root. If it cannot, say so in the report.

**Spec 03** (same commit), § Layout, after "A local object write lands in the destination directory as `.<filename>.tmp-<uuidv7>`, then `sync_all`, rename onto `<filename>`, and an fsync of that directory.": add "A write, sync or rename that fails removes the temp file."

**`hardening.md`**: the finding `M5-1` (§ R5.8 format).

**CHANGELOG** `[Unreleased]` → `Fixed`: "A local object write that fails, for example on a full disk, no longer leaves its temporary file until the next start."

### Commands

```bash
just resources
just check
```

---

## R5.6 — Memory limits

Commit: `storage: size query memory from the process cgroup and test memory limits (R5.6)`

### The fix: the process's own cgroup limit

`memory.rs::default_total_memory` reads the limit of the process's cgroup and its ancestors, instead of the cgroup mount's root:

```rust
pub(crate) fn default_total_memory() -> u64 {
    use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
    let mut system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    let mut total = system.total_memory();
    if let Ok(pid) = sysinfo::get_current_pid() {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, ProcessRefreshKind::nothing());
        if let Some(limits) = system.process(pid).and_then(sysinfo::Process::cgroup_limits)
            && limits.total_memory > 0
        {
            total = total.min(limits.total_memory);
        }
    }
    (total / 2).min(8 << 30)
}
```

`Process::cgroup_limits` (`limits_for_process` in sysinfo) joins the `0::<path>` line of `/proc/<pid>/cgroup` to `/sys/fs/cgroup` and takes the lowest `memory.max` from there up to the root. Inside a container with its own cgroup namespace the path is `/`, which gives today's reading; on a host it finds the systemd unit's or scope's `MemoryMax=`. Check that these names exist in sysinfo 0.39.6 with the `system` feature (`src/common/system.rs`); if one differs, use the real one and report it.

Split it in two so the open-time check below can reuse the limit: `pub(crate) fn detected_memory() -> u64` returns `total` as computed above (RAM, or the cgroup limit when lower), and `default_total_memory()` becomes `(detected_memory() / 2).min(8 << 30)`.

`lib.rs::resolve_query_memory` already logs when it lowers the cap. Add one `tracing::info!(total, "query memory pool")` with the resolved total, so an operator sees which limit was used.

There is no unit test for the detection: sysinfo reads fixed paths. `child_sees_the_scope_limit` below is its regression test.

### The open-time warning (decided: Q1 (a))

The budgets are not auto-sized. Instead, `Storage::open` warns when they add up to more than the memory it can see. The arithmetic is a pure function in `memory.rs`:

```rust
/// Base memory of the engine outside every budget (runtimes, catalog, caches' indexes).
pub(crate) const BASE_BYTES: u64 = 200 << 20;

/// What the storage's budgets can hold at once (spec 03 § Resource needs):
/// 2 × memtable.hard_limit (spans and scores, each on its own)
/// + 2 × flush.max_bytes (the copy each table's flush sorts)
/// + compaction.max_memory + query.metadata_cache_bytes + query_total + BASE_BYTES.
/// Saturating.
pub(crate) fn budget_bytes(config: &StorageConfig, query_total: u64) -> u64;
```

In `open_inner`, after `resolve_query_memory`: `let budget = memory::budget_bytes(&config, total_memory as u64)` and `let detected = memory::detected_memory()`. If `budget > detected`, log `tracing::warn!(budget, detected, "the storage memory budgets exceed the memory limit; the process can be killed under load (spec 03 § Resource needs)")`. It is a warning, not an error: the budgets are upper bounds that a light load never reaches.

Unit tests in `memory.rs`:
- `the_budget_adds_every_component`: a `StorageConfig` literal written as `crash_tests.rs::quiet_config` writes one (that helper and `lib.rs`'s `config_in` are private to their test modules), with `memtable.hard_limit` 1 GiB, `flush.max_bytes` 256 MiB, `compaction.max_memory` 1 GiB, `metadata_cache_bytes` 512 MiB. `budget_bytes(&config, 2 GiB)` is `2 GiB + 512 MiB + 1 GiB + 512 MiB + 2 GiB + 200 MiB`.
- `the_budget_saturates`: `memtable.hard_limit` at `ByteSize(u64::MAX / 2)` gives `u64::MAX`, not a panic.

### CHANGELOG

`[Unreleased]` → `Fixed`: "The default query memory pool now honours a systemd `MemoryMax=` (or any cgroup limit of the process), not only a container's limit." → `Added`: "A warning at start when the storage's memory budgets add up to more than the memory limit."

### `model_tests/resources/memory.rs`

A **child** runs a heavy workload in its own process. A **parent** re-executes it under `systemd-run --user --scope` with a memory limit, and checks that it was not killed.

```rust
/// What the child configures. `Probe` only reports the limit it sees.
#[derive(Debug, Clone, Copy)]
enum Profile { Defaults, Small, Probe }

/// Every default budget, for `data_dir`: what `llmobs serve` runs with.
fn defaults_config(data_dir: &Path) -> StorageConfig;
/// The small-machine profile (below).
fn small_config(data_dir: &Path) -> StorageConfig;

/// The child was killed by the kernel.
struct Killed;
/// Runs the child under `MemoryMax=limit`, `MemorySwapMax=0`. `Ok(result line)` when
/// it finished, `Err(Killed)` when it was killed.
fn run_limited(profile: Profile, limit: u64) -> Result<String, Killed>;
```

- **`defaults_config`.** `min_free_bytes` 2 GiB, every sub-config `Default::default()`, so every memory budget is the default:
  - `memtable.hard_limit` 1 GiB, which applies **per table** (`Storage::append` compares each writer's bytes);
  - `flush.max_bytes` 256 MiB with `upload_concurrency` 8;
  - `compaction.max_memory` 1 GiB;
  - `query.max_memory_total` unset (50% of the limit), `max_memory` 2 GiB;
  - `metadata_cache_bytes` 512 MiB.

  Only timers change, so the run exercises everything within a minute: `flush.max_age` 10 s, `compaction.interval` 5 s, `compaction.l0_files` 2. `StorageConfig` has no `Default`: write the literal as `crash_tests.rs::quiet_config` does.
- **`small_config`**, the profile R5.8 documents for a machine with about 1 GiB:
  - `memtable.hard_limit` 128 MiB;
  - `flush.max_bytes` 32 MiB, `upload_concurrency` 2;
  - `compaction.max_memory` 128 MiB, `compaction.threads` 1, `target_file_size` and `l0_bytes` 128 MiB;
  - `query.max_memory_total` 256 MiB, `max_memory` 128 MiB, `initial_memory` 16 MiB, `metadata_cache_bytes` 64 MiB, `target_partitions` 2.

  Timers as in the defaults.
- **The data directory** must not be on tmpfs: tmpfs pages are charged to the writer's cgroup and cannot be reclaimed without swap, so WAL and Parquet bytes would count as memory. Use `<target dir>/llmobs-resources/<uuid v7>`, where the target dir is `current_exe().ancestors().nth(3)` (the binary is `target/<profile>/deps/<exe>`, so `nth(3)` is `target/`). The parent asserts `rustix::fs::statfs(dir).f_type != 0x0102_1994` (`TMPFS_MAGIC`) and removes the directory afterwards. `f_type`'s integer type differs between targets: cast both sides as the compiler asks (for example `as i64`). A disk-backed file's page cache is reclaimable, so it does not cause an OOM kill.
- **`run_limited`** spawns:

  ```text
  systemd-run --user --scope --quiet --collect -p MemoryMax=<limit> -p MemorySwapMax=0 \
      <current_exe> --exact model_tests::resources::memory::child_run --ignored --nocapture --test-threads=1
  ```

  with env `LLMOBS_MEMORY_CHILD_DIR`, `LLMOBS_MEMORY_CHILD_LIMIT` (bytes) and `LLMOBS_MEMORY_CHILD_PROFILE` (`defaults`, `small` or `probe`). Stdout is piped (the child prints one `LLMOBS_MEMORY …` line), stderr inherited, with a 600 s timeout. `status.signal() == Some(9)` (`std::os::unix::process::ExitStatusExt`) or `status.code() == Some(137)` is `Killed`. Any other non-success panics with the child's last 40 lines of stdout. `systemd-run` failing to start (no user bus: "Failed to connect to bus") panics with "needs a systemd user session (systemd-run --user)".

### The child (`child_run`, ignored, returns at once without `LLMOBS_MEMORY_CHILD_DIR`)

1. `probe` profile: print `LLMOBS_MEMORY total=<default_total_memory()> cgroup=<the 0:: line of /proc/self/cgroup>` and return.
2. Build the profile's config. If `memory::default_total_memory()` is above `limit / 2 + 1 MiB`, the process cannot see its cgroup limit (a cgroup namespace whose root is not the mounted one, or cgroup v1): print `LLMOBS_MEMORY note=limit-not-visible`, and set `query.max_memory_total = Some(limit / 2)`, which mirrors the default rule. The run still tests every other budget.
3. `Harness::with_options` with `root` = the data dir, `workers` = `std::thread::available_parallelism()`, and an `adjust` that replaces the config with the profile's (keeping the harness's `data_dir`).
4. `model_steps(seed_ops())`.
5. **How the loops run.** One `h.block_on(async { … })` drives every loop of a phase with `tokio::join!` (or a `futures::future::join_all` over a `Vec` of loops), so they all run on the life's runtime and borrow `h.storage()` without `Arc`. Each loop checks a shared `stop: AtomicBool` before every call and returns its tally when it is set. A timer future in the same `join!` sets `stop` at the phase's deadline. Batch numbers come from a shared `AtomicU32` counter, passed to `filler_batch` as `counter % 60_000`, with `day_offset = counter % 8`. Arrow bytes appended so far are summed in a shared `AtomicU64`.
6. **Fill (at most 60 s).** `hooks().hold(HookPoint::FlushAfterStage)`. Four appender loops add filler batches (`Text::Words`, 64 × 16 KiB, about 1 MiB each) as fast as they can, each until it has seen `AppendError::Backpressure`. The spans memtable is then at its hard limit, with one flush frozen and paused. The phase ends when all four have seen it, or at 60 s.
7. **Load (60 s).** Release the hook. Four appender loops go on: a `Backpressure` waits 50 ms and retries. They also stop once the shared byte count passes 6 GiB, to bound the disk use. Four query loops run alongside, each query with a 30 s timeout; loop `i` runs query `i % 3`:
   - `reads::trace_list(&ctx, TraceListRequest { filter: None, search: None, scope: ListScope { window: Some(TimeWindow::new(midnight(day(0)), midnight(day(8))).expect("window")), include_eval: true, include_experiments: true }, sort: TraceSort::CostTotal, order: SortOrder::Desc, limit: 50, cursor: None, timeout: Duration::from_secs(30) })`, with `ctx = QueryContext::new(storage.query(filler(), QueryOptions::default()).await?, None)`. A cost sort runs the full merge (`benchmarks.md` § Findings (a)). If `trace_list` rejects the 8-day window as too wide (`RunError::InvalidRequest`), use the widest window it accepts and report it;
   - `sql_json`: `SELECT input, count(*) AS n FROM spans GROUP BY input ORDER BY n DESC`, limit 10: a hash aggregate that spills;
   - R5.2's window count.

   Background compaction runs (5 s interval).
8. **Tally.** Appends: `Ok`, `Backpressure`, `DiskFull`, and anything else. Queries: `Ok`, `Err(QueryError::Busy)` from `Storage::query`, `RunError::TooLarge`, `RunError::Timeout`, and anything else. **Anything else fails the child**: print its kind (`RunError::kind`, the `AppendError` variant) and panic.
9. After the `block_on` returns (every loop has stopped): `h.restart()`, then `check_full(h, model, 500)` (A/B). No settle: the filler is gigabytes.
10. Print one line: `LLMOBS_MEMORY profile=<p> limit=<bytes> peak_rss=<VmHWM bytes> appends_ok=… backpressure=… queries_ok=… busy=… too_large=… timeout=…`. `VmHWM` comes from `/proc/self/status`.

### Parents

- `defaults_at_the_documented_minimum` (ignored): `run_limited(Profile::Defaults, MIN_MEMORY_DEFAULTS)` is `Ok`.
- `small_profile_at_its_minimum` (ignored): `run_limited(Profile::Small, MIN_MEMORY_SMALL)` is `Ok`.
- `minimum_memory` (ignored, run by its own recipe). The profile comes from `LLMOBS_MEMORY_PROFILE` (default `defaults`). It runs `run_limited` at decreasing limits until the first `Killed`:
  - `defaults`: 12, 10, 8, 7, 6, 5, 4 and 3 GiB;
  - `small`: 2 GiB, 1.5 GiB, 1 GiB, 768 MiB and 512 MiB.

  Then two bisection steps between the last pass and the first kill. It prints every run's line and the smallest passing limit. The machine needs that much free RAM: check `free -g` first, and report the machine.
- `child_sees_the_scope_limit` (ignored): `run_limited(Profile::Probe, 2 GiB)`. When the printed cgroup path does not start with `0::/..`, assert the printed total is 1 GiB. Otherwise print "cgroup path not resolvable here (cgroup namespace)" and pass. That prefix is how a cgroup namespace whose root is not the mounted one shows (this sandbox).

`MIN_MEMORY_DEFAULTS` and `MIN_MEMORY_SMALL` are `const u64` in the test module `model_tests/resources/memory.rs` (not in production `crate::memory`; the child calls the production functions as `crate::memory::default_total_memory()` and `crate::memory::detected_memory()`), set from the measurement: the smallest passing limit × 1.25, rounded up to a multiple of 256 MiB. A rough budget, to check the measurement against: the two memtables (2 × 1 GiB), the frozen copy a flush sorts, compaction (1 GiB), the metadata cache (512 MiB), about 200 MiB of base, and query memory at half the limit put the defaults near 8 GiB in the worst case. If the measurement is far from that, say why in the report.

### `justfile`

```just
# Memory-limit runs (track R M5, D-R-9), release mode, under a systemd user scope.
resources-memory:
    cargo test --release --workspace --all-features -- --ignored --test-threads=1 --exact model_tests::resources::memory::defaults_at_the_documented_minimum model_tests::resources::memory::small_profile_at_its_minimum model_tests::resources::memory::child_sees_the_scope_limit

# The smallest memory limit a profile survives (defaults or small). Takes tens of minutes.
resources-memory-min profile="defaults":
    LLMOBS_MEMORY_PROFILE={{profile}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::resources::memory::minimum_memory
```

### Commands

These runs need **at least 10 GiB of free disk** under `target/` (each run ingests up to 6 GiB of Arrow; WAL and Parquet take a part of that, and the directory is removed after each run) and the free RAM of the largest limit tried. Check `df -h target` and `free -g` first, and report both.

```bash
# In the background, each with its output in a log: they take long.
just resources-memory-min defaults > <scratchpad>/m5-memory-defaults.log 2>&1
just resources-memory-min small > <scratchpad>/m5-memory-small.log 2>&1
# Set MIN_MEMORY_DEFAULTS and MIN_MEMORY_SMALL from the logs, then:
just resources-memory
just check
```

An OOM kill at a limit above the rough budget is a finding: a memory consumer outside every budget. Look at the child's last lines and tallies, and record it for R5.8. A child that fails on "anything else" is a finding too.

---

## R5.7 — File descriptors

Commit: `storage: test the storage under a low file descriptor limit (R5.7)`

### What the engine holds open

From the code, to be checked against the measurement:

| Holder | Descriptors |
|---|---|
| `LOCK` | 1, for the process's life |
| `catalog.db` (one sqlx connection, WAL mode) | 3: the database, `-wal`, `-shm` |
| WAL writers | 1 open segment per table (2), plus 1 for a moment when a segment is created (directory fsync) |
| Flush | up to `flush.upload_concurrency` temp files per table at once, plus 1 per directory fsync |
| Compaction | 1 output temp file per running job (`compaction.threads`); input reads open and close per range (`LocalFileSystem`) |
| Queries | range reads open and close per call; each spill file is open while written, and a spill merge opens all of a sort's spill files at once (`max_spill_merge_fan_in` is 0, unlimited) |
| Read cache | one file per block read or write, opened and closed |
| Tokio, sqlx, libtest | a few per runtime and worker |

The server adds `meta.db` (four read connections and one writer, three files each), one socket per client connection, and outbound HTTP connections. Spec 03 counts the storage only, and says so.

### `model_tests/resources/descriptors.rs`

```rust
/// Entries of /proc/self/fd, minus the one `read_dir` itself uses. `None`
/// when `read_dir` fails (EMFILE under a low limit): the sampler skips that
/// sample instead of panicking.
fn open_fds() -> Option<usize>;
/// `setrlimit(Resource::Nofile, Rlimit { current: Some(soft), maximum: <current hard> })`.
fn set_soft_nofile(soft: u64);
/// What the child does.
#[derive(Debug, Clone, Copy)]
enum ChildMode {
    /// Open and seed at the normal limit, lower it for the load phase only.
    Load,
    /// Lower the limit first, then only open the storage.
    Open,
}
/// Re-executes `child_run` with `LLMOBS_FD_CHILD_MODE` (`load` | `open`),
/// `LLMOBS_FD_CHILD_LIMIT` (0: keep the current limit) and
/// `LLMOBS_FD_CHILD_SECONDS`, and returns its `LLMOBS_FD` line.
fn run_child(mode: ChildMode, limit: u64, seconds: u64) -> String;
```

`run_child` works as `crash_tests.rs::run_child`, without a namespace: `Command::new(current_exe()) --exact model_tests::resources::descriptors::child_run --ignored --nocapture --test-threads=1`, stdout piped, a 300 s timeout, and it asserts `status.success()`.

**The data directory** is under `current_exe().ancestors().nth(3)` (`target/`) `/llmobs-resources/`, as in R5.6 (`HarnessOptions::root` set explicitly, never the system temp dir), removed by the parent afterwards.

**The config**, `model_config` with:
- `flush.max_bytes` 1 MiB, `flush.max_age` 1 s (background flushes);
- `compaction.interval` 1 s, `l0_files` 2, `threads` 4;
- `wal.segment_bytes` 1 MiB (frequent rotations);
- `query.target_partitions` 8, `query.max_memory` 16 MiB, `query.initial_memory` 8 MiB (queries spill). `validate` requires `initial_memory ≤ max_memory` (`config.rs`), and the default `initial_memory` is 64 MiB.

**The child in `Open` mode** (`child_run`, ignored, returns at once without `LLMOBS_FD_CHILD_MODE`):
1. Build the tokio runtime first (a multi-thread runtime opens its own descriptors: epoll, an eventfd), with the data directory already created at the normal limit.
2. Then `set_soft_nofile(limit)`.
3. Then `runtime.block_on(Storage::open(config))`. `Ok`: shut it down cleanly. `Err`, by `OpenError` variant (`error.rs`):
   - **accepted** as a clean refusal: `Io`, `Objects`, `Catalog`, `WalReplay`, `Anchor`;
   - **panic** on `Config` (the test's config is wrong, and every run would pass vacuously), `Locked`, `Runtime`, and `WalCorrupt`: running out of descriptors must never be reported as corruption.
   - If a `WalReplay` error turns out to wrap `EMFILE` as corruption (a `WalError::Corrupt`, or a message that calls the segment corrupt), record that as a finding for R5.8 with the error's `Debug`; the case still passes.
4. Print `LLMOBS_FD mode=open limit=<n> open=<ok|failed> kind=<variant>`.

**The child in `Load` mode:**
1. `let original = getrlimit(Resource::Nofile).current`. Build the harness at the normal limit: `Harness::with_options` (root as above, `workers` 4) with the config. Opening must succeed here; a `Config` error panics with "the descriptor test's config is invalid".
2. `model_steps(seed_ops())`. Record `baseline = open_fds()`.
3. **Lower the limit for the load phase only:** when `limit` is not 0, `set_soft_nofile(limit)`. M1's `apply` and the `Harness` methods `expect` success, so none of them is called until the limit is restored. The loops below call `h.storage()` methods directly and tally every `Err`.
4. **Load** for `seconds`, run as in R5.6 (one `h.block_on`, `tokio::join!` of the loops, a shared stop flag set by a timer, a shared batch counter passed as `counter % 60_000`, and a total cap of 1 GiB of Arrow):
   - a sampler loop recording the peak of `open_fds()` every 10 ms, skipping `None`;
   - four appender loops: filler batches (`Text::Words`, 32 × 4 KiB) across 60 days (`day_offset = counter % 60`), so flushes write many partitions and compaction runs many jobs at once;
   - four query loops: R5.2's window count over project C (it spills, across many files), and `trace_list` on C with the `TraceListRequest` of R5.6 (window `day(0)..day(60)`, or the widest the read accepts);
   - one loop calling `storage.compact(*clock)` and counting `report.failed` and `Err`s.

   Every call runs under `tokio::time::timeout(60 s)`. A timeout is a **wedge** and panics the child. Tally the outcomes as in R5.6. Accept for appends: `Ok`, `Backpressure`, `WalFailed`. For queries: `Ok`, `TooLarge`, `Busy`, `Timeout`, `Failed`, `Internal`. A panic anywhere fails the child.
5. Restore the limit: `set_soft_nofile(original)`. Wait 3 s.
6. If `health().spans_wal_failed`, record `wal_failed=true`; then `crash_then_try_open()` must be `Ok`. Otherwise one more A append through `apply` must be `Ok`.
7. `h.restart()`, `check_full(h, model, 500)`. `h.block_on(h.storage().verify_pass()).quarantined == 0`: no file was quarantined by a descriptor error. `settle_and_check`.
8. Print `LLMOBS_FD mode=load limit=<n> baseline=<b> peak=<p> appends_ok=… wal_failed=<bool> queries_ok=… query_errors=<kind:count,…> compaction_failed=…`.

### Parents

- `peak_at_the_default_limit` (ignored): `run_child(ChildMode::Load, 0, 30)`. Assert `peak < 1024`. 1024 is the soft limit systemd services get by default (`LimitNOFILE=1024:524288`), and what recent Docker releases give containers. Above it is a finding: when the sampler sees a new peak, it also counts the descriptor targets by kind (regular file, socket, `anon_inode`, pipe) from `read_link`; print them.
- `a_low_limit_gives_errors_not_panics` (ignored): `run_child(ChildMode::Load, limit, 20)` for `limit` in `[256, 128, 64, 32]`. Every child exits successfully. Print every result line.
- `opening_under_a_low_limit_is_a_clean_error` (ignored): `run_child(ChildMode::Open, limit, 0)` for `limit` in `[64, 32, 16, 8]`. Every child exits successfully. Print which limits opened and which refused.
- `a_low_limit_smoke` (ignored, also in `just test`): `run_child(ChildMode::Load, 64, 5)`. It takes about 10 s in debug and keeps "errors, not panics" in every `just check`. If it fails in R5.7 for a reason other than a `WalFailed` (which the child accepts), do not add it to `just test`: keep it ignored, record the failure as a finding for R5.8, and add it to `just test` once R5.8 fixes it.

### `justfile`

`resources` already includes `descriptors::`. In `test:`, after the crash-test lines:

```just
    cargo test --workspace --all-features -- --ignored --exact model_tests::resources::descriptors::a_low_limit_smoke
```

### Commands

```bash
just resources
just check
```

Record in the report:
- the baseline and peak at the default limit;
- for each low limit: which components failed, and whether the WAL writer failed.

A `WalFailed` caused by `EMFILE` is a finding for R5.8, not a test failure. So is a peak at or above 1024.

---

## R5.8 — Long runs, fixes, limits for operators

Commit per bug: `storage: <fix in imperative> (R5.8)`. A final docs commit: `docs: record the M5 resource runs and limits (R5.8)`.

### Runs

1. `just resources` three times, plus once with `LLMOBS_TEST_SMALL_FS` on an ext4 loop device if `sudo` is available (R5.1's commands). If it is not, say so; the user can run it.
2. `just resources-memory`, and `just resources-memory-min` for both profiles once more at the final commit.
3. `a_low_limit_gives_errors_not_panics` at `[1024, 512]` as well, for the record (a temporary local change of the list, not committed).

Long runs go in the background with their output in a log file under the scratchpad, as in M1's R1.8.

### For each failure

Follow M1's R1.8 procedure:
1. Keep the case's output. The cases are deterministic apart from timing: if a failure does not repeat in 5 runs, record it as flaky with its output, and stop and report.
2. Decide whose bug it is: the test stepped outside a spec guarantee (fix the test and cite the spec line), or the storage disagrees with spec 03 (fix it).
3. A storage fix that is local (one module, no format, public API or spec change) goes in one commit with the case as its regression test. Otherwise stop and report the case, the cause and the change it needs.
4. Add a row to `hardening.md` § M5 Findings.

### Expected findings and what to do

- **The read cache outruns the monitor (D7's sampler went under `min_free_bytes − 1 MiB`).** A local fix: `ReadCache::write_blocks` measures before writing (`self.disk.refresh()` once per call, one `statvfs`), instead of trusting the last measurement. Regression tests:
  - D7;
  - a deterministic unit test in `read_cache.rs`, `writes_check_free_disk_now_not_the_last_measurement`, in `just test`, committed with the fix. Build a `ReadCache` over a `Disk` with `min_free = 1 MiB`, record an object's size, and call `disk.set_free_for_test(Some(10 MiB))`: the stored measurement says writable. Then make the next measurement report `Some(1 MiB)` without refreshing the stored value. Add `#[cfg(test)] fn set_next_measurement_for_test(&self, free: Option<u64>)` to `Disk`: it sets `fake_free` without calling `refresh`. Call `write_blocks` with one block. Assert the block is not cached (`read_blocks` returns nothing for it).
  - Before the fix the unit test fails (the block is written on the stale measurement); after it, it passes. Write the test first and confirm it fails.
- **Spill merges exhaust descriptors** (R5.7: `EMFILE` in queries at 1024, or a peak over 1024 from spill files). A local fix: `build_runtime` sets `.with_max_spill_merge_fan_in(16)`. It bounds the open spill files per merge at the cost of extra merge passes. Regression test: `peak_at_the_default_limit`. Record the peaks before and after.
- **The WAL writer fails on `EMFILE` or inode exhaustion at a rotation** (D9, R5.7). That is today's rule ("write errors are final", spec 03 §1) applied to a step that lost nothing: the group was written and synced to the old segment, then `IoJob::commit` rewinds it because the new segment could not be created. The user chose (Q3) to make such a failure not final, but changing it is still a spec 03 §1 rule change: **stop and report** it to the coordinator with the proposed decision below, which the coordinator numbers and records. Do not change `wal/writer.rs` in M5 until it is recorded.

  > **Proposed D-R-x — WAL rotation.** A WAL segment rotation that fails is not final. When the group was written and synced to the current segment, the writer acknowledges it, keeps that segment open, logs the failure (kind only), and tries the rotation again after the next group. The segment then grows past `wal.segment_bytes` until a rotation succeeds. Only a failed write or `fdatasync` of the segment itself stops the writer (`WalFailed`), because only those can lose acknowledged bytes after a writeback error (D-R-6). A segment that cannot be created loses nothing: the group's bytes are already durable in the old segment. Running out of file descriptors or inodes, which are transient, then no longer forces a restart. Spec 03 §1 *Write errors are final* gets the sentence: "A segment that cannot be created is not a write error: the writer keeps the current segment and retries the rotation after the next group." The test is D9 and R5.7's low-limit runs: no `WalFailed` without a failed write or sync.
- **`catalog.db` full (`SQLITE_FULL`, code 13) stops the committer.** That happens if M2 classified every database error of a commit as a fatal I/O error. SQLite rolls a `SQLITE_FULL` transaction back cleanly; only `SQLITE_IOERR_*` follows a failed fsync. Stop and report: the classification is M2's (D-R-6).
- **An OOM kill above the rough budget.** A memory consumer outside every budget. Find it (the child's tallies, `VmHWM`, a heap profile if needed); stop and report unless the fix is a local bound.

### `docs/research/storage/hardening.md` § M5

Add `## M5 — Resource exhaustion` with:
- **What the tests cover**: one list, D1–D9, the spill cap, the memory runs, the descriptor runs.
- **Environment**: the small filesystem (unprivileged `unshare -rm` tmpfs, and what tmpfs misses, from R5.1), the ext4 loop setup, the systemd user scope, `RLIMIT_NOFILE` in a re-executed child, and whether the machine runs in a cgroup namespace (the `0::/..` path).
- **`### Runs`**: date, commit, recipe, mode, machine, duration, result.
- **`### Measured limits`**:
  - memory: the smallest passing limit and the documented minimum for both profiles, with peak RSS;
  - descriptors: baseline and peak, and the lowest limit that runs without errors.
- **`### Findings`**: `M5-<n>`, the case, the cause, the fix commit. `M5-1` is R5.5's temp files. The uncapped spill (known from `benchmarks.md` § Findings (a), fixed in R5.2) and the ignored systemd memory limit (fixed in R5.6) are `M5-2` and `M5-3`.

### Spec 03: limits for operators

The engine's numbers belong in spec 03, next to the behaviour they bound; spec 12 points to them. `docs/self-hosting/` belongs to track N: M5 does not edit it, and the report asks for its changes.

- **§2 Backpressure**: "if `active + frozen` exceeds `memtable.hard_limit`" becomes "if a table's `active + frozen` exceeds `memtable.hard_limit` (each table on its own, so both together can hold twice that)".
- **§10 table**, new rows (the expected text; adjust it to the results):

  | Situation | Behaviour |
  |---|---|
  | The disk fills before the monitor sees it (`ENOSPC`) | WAL: `WalFailed`, final (§1). Flush: retried with backoff (§2). Compaction: the job fails, and a later pass redoes it. `put_media`: the call fails, and GC collects the staged row (§9). Read cache: the block is not cached. Query spill: too large (422). A failed write removes its temp file. Once space is free, the next attempt succeeds. Inodes are not monitored: running out of them behaves the same way |
  | Out of file descriptors (`EMFILE`) | The step that opened the file fails as for an I/O error: a query fails, and a flush or compaction is retried. *(Provisional, pending the coordinator's decision on WAL rotation: until it is recorded, an append is refused when a WAL segment cannot be created, then `WalFailed`, §1. Once it is recorded, the writer keeps the current segment instead.)* Nothing is quarantined |
  | Memory limit | Below the documented minimum (§ Resource needs) the kernel can kill the process. At or above it, the budgets refuse work instead: `Backpressure` (503), `Busy` (503), too large (422) |

- **New `### Resource needs`** at the end of §10:
  - **Disk:** `storage.min_free_bytes` must cover at least one second of the peak write rate (WAL plus flush and compaction outputs), because free disk is measured once a second. The default 2 GiB covers about 2 GB/s.
  - **Memory:** the budgets add up as:
    - `2 × memtable.hard_limit` (spans and scores);
    - plus the copy a flush makes while sorting (up to `flush.max_bytes` per table);
    - plus `compaction.max_memory`;
    - plus `query.metadata_cache_bytes`;
    - plus the query pool (`query.max_memory_total`, by default half the limit);
    - plus about 200 MiB of base.

    The measured minimum with every default is `<MIN_MEMORY_DEFAULTS>`. The small profile (its keys and values, from R5.6) runs in `<MIN_MEMORY_SMALL>`. The budgets are not sized from the machine; `Storage::open` logs a warning when they add up to more than the memory limit it detects (R5.6). The server adds `ingest.max_inflight_bytes` and its own needs (spec 12).
  - **File descriptors:** R5.7's table with the measured baseline and peak, and a recommended soft limit for the storage: at least 4,096, or the measured peak × 4, whichever is larger. The server's sockets come on top.
  - **Tested by** `just resources` and `just resources-memory` (track R M5).

### Spec 12

In § Configuration, after the paragraph below the keys table that starts "The storage engine's settings form the `[storage]` section", one sentence: "Memory, disk and file-descriptor needs of the storage engine, and a profile for small machines, are in spec 03 § 10 Resource needs."

### Requests (in the report, not edited here)

- **Track N, `docs/self-hosting/`:**
  - a "Sizing" section or page with the memory minimum, the small profile, `min_free_bytes` and the descriptor limit;
  - in `install.md`'s systemd unit, `LimitNOFILE=` at the recommended value, and an optional `MemoryMax=` (now honoured by the query pool);
  - Helm `values.yaml` resource requests and limits at the documented minimum.
- **The server/CLI (decided, Q2):** `llmobs serve` raises its soft `RLIMIT_NOFILE` to the hard limit at start, as Go programs do, and logs both values. This is the coordinator's to assign (it is not R code). Spec 03 § Resource needs states the storage's needs either way.
- **The server's OpenAPI text** for `query_too_large` ("needed more memory than allowed"): add "or temporary disk space".
- **The coordinator: the proposed WAL-rotation decision**, if R5.3 D9 or R5.7 showed a `WalFailed` from a failed rotation.

### Wrap-up

- Remove `#![allow(dead_code)]` from `model_tests/resources/mod.rs`, and delete what clippy then reports unused.
- `docs/roadmap.md`, track R row: "M5 done".

---

## What not to build in M5

- **A fixed spill size key** (`query.max_spill_bytes`): the cap follows `min_free_bytes`, which the operator already sets.
- **Auto-sized memory budgets** (defaults that scale with the detected limit): decided against for now (Q1 (a)). M5 documents a minimum and a small profile, and warns at open.
- **Inode monitoring** in `Disk`: D9 records the behaviour.
- **Changes to the WAL rotation rule** without the coordinator's decision.
- **Server changes**: the descriptor limit raise at start, the OpenAPI wording of `query_too_large`, ingest memory (`ingest.max_inflight_bytes`) under a cgroup.
- **`dm-flakey`, `dm-error` or a FUSE filesystem** to inject `ENOSPC` at writeback: that needs root, and M2's fsync-error injection covers the path.
- **Network and S3 resources** (sockets, connection pools, S3 throttling): M4.
- **Soak-level leak detection** (descriptors or memory growing over hours): M7.
- **A CI or nightly job** for the disk-full, memory or descriptor runs (decided, Q4): `just resources*` run by hand. Only `a_low_limit_smoke` and R5.2's unit tests run in `just test`.
