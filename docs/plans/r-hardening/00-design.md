# Track R — Storage hardening: design

`crates/storage` works and is fast: A1 built the WAL, memtable, Parquet files, catalog, compaction and the S3 read cache; A2 the query layer; M deletion and retention. The benchmarks reached about 83,000 spans/s of ingest, and every query target passed on 1 million traces (`docs/research/storage/benchmarks.md`). That is not yet proof that the engine is production ready. Track R provides that proof, and shows the evidence publicly.

**Goal.** Show, with tests that anyone can rerun, that the storage engine:
1. never loses an acknowledged write, including on power loss;
2. never returns a wrong result;
3. never panics or wedges on bad bytes, failing disks, failing S3, or exhausted resources;
4. stays stable under sustained load at scale.

The results are written up so they can back a "fast and solid" claim.

**Decisions:** `D-R-1`–`D-R-14` in `docs/mvp-decisions.md` § Track R. **Plans:** one per milestone in this directory. Each plan is written when its milestone starts; `01-m1-model.md` is the first.

## What exists today

| Area | What A1/A2/M left | Gap |
|---|---|---|
| Crash recovery | `crash_tests.rs` re-executes the test binary and aborts it at hook points (`hooks.rs`, 24 points) in flush, compaction, deletion, media and drop. The parent reopens the directory and checks row ids, file counts and deletion coverage. `tests/crash.rs` is a kill -9 harness: random kills during ingest with background flush and compaction on, checking acknowledged rows, catalog files and source pairs. | An abort keeps the OS page cache, so data that was never `fsync`ed survives. **No test loses an unsynced write.** No crash in the middle of an operation is followed by a check of the full read model (traces-view values, scores, sessions, metrics): M1 crashes only between completed ops (full-plan review R-1, closed by M2). |
| Model test | `compaction/model.rs`: proptest, 16 cases of up to 40 ops (append one span, restart, compact, GC, delete a trace, apply deletions, retention). It checks the `(trace_id, span_id, ingest_time)` multiset of the raw `spans` provider, and `span_count` per trace once everything is settled. | One table, one column family, raw providers. No scores, no `traces` view values, no read API (lists, lookups, metrics, sessions, users), no late spans, no crash without a clean shutdown, no second project. |
| Corruption | Quarantine and verify (D-A1-10), CRC-32C on WAL records and read-cache blocks, `quarantine_tests.rs`. | No fuzzing of any decoder, ours or `parquet`'s. |
| S3 | `test_support::FaultStore` fails puts or reads on demand; `tests/s3.rs` runs against Versity Gateway; the multipart bug of M6.6 was found by a benchmark. | No latency, 503, connection resets, partial multipart or listing faults. Never run against a remote endpoint. |
| Resources | `min_free_bytes` refuses appends; compaction reserves disk; query memory admission (D-A1-8, D-A1-9). | Never tested on a really full disk. Query spill files are not checked against `min_free_bytes` (`benchmarks.md` § Findings (a)). No memory-cgroup or file-descriptor limit tests. |
| Scale | 1M-trace `trace_bench`, 60 s ingest bench, on a workstation with a rotational disk and a loopback S3. | No 5M run, no soak, no network S3, and payloads are random words, so compression is not realistic. |
| Format | `llmobs.schema_version` in Parquet metadata, WAL record `kind`, forward-only `sqlx` migrations for `catalog.db`. | No rule for what needs a format version, and no test that opens an older directory. |

## Audit so far

Reading the durability paths for this design (`wal/segment.rs`, `wal/writer.rs`, `wal/replay.rs`, `objects.rs`, `gc.rs`, `read_cache.rs`) found candidate bugs. M2 confirms each one with a lossy-crash test before fixing it.

1. **WAL rotation in interval mode** (`wal.sync = "<duration>"`). `IoJob::commit` writes a group and rotates without syncing the old segment, then drops it, so that segment's tail stays dirty. On power loss the old segment is torn but is no longer the last one. Replay treats a bad record in a non-final segment as corruption and **refuses to start** (spec 03 §1 Recovery). The default `"always"` mode is not affected.
2. **GC unlink is not made durable.** `collect_obsolete` deletes a local object, then its `data_files` row, with no directory fsync in between. After a power loss the file can come back with no row pointing to it. Nothing lists the object tree today, so the file leaks forever. N's `collect_orphans` (N M2, B5) will reclaim such files when an operator runs `llmobs admin gc-orphans`, but the leak itself is a durability bug and M2 fixes it.
3. **Failed WAL group.** `discard_unacked` removes a new empty segment without a directory fsync. It also runs `rewind` (`set_len` + `sync_all`) after a failed `fdatasync`, which is a second fsync after the first one failed. The writer stops either way. After a restart, an unacknowledged group may still be on disk and be replayed. That is acceptable (the client got an error and may retry, and duplicates are tolerated), but it must be documented and tested.
4. **Read cache `size` file.** It is written through `write_renamed` with no fsync and no checksum. A torn write that still parses as a number would give a wrong object size. That size bounds block reads, so it could serve short or wrong ranges. The blocks themselves carry a CRC.
5. **Catalog fsync errors.** SQLite reports `SQLITE_IOERR_FSYNC`, but nothing defines what the committer does next. Today a failed commit is a retryable job error, which repeats the fsyncgate mistake on `catalog.db`.

## How others harden their engines

What we take from each (research brief of this design session; no code copied):

- **PostgreSQL fsyncgate (2018).** After a writeback error, Linux may mark the dirty pages clean, so a retried fsync can report success after the data was lost. PostgreSQL now PANICs on any fsync failure and recovers from its WAL; InnoDB and WiredTiger did the same. We already do this for the WAL (spec 03 §1). **R extends it to every durable write** (D-R-6).
- **ALICE (OSDI'14), CrashMonkey/ACE (OSDI'18).** Applications break on crash-state assumptions: a rename that is not durable without a directory fsync, a new file that needs its parent directory synced, appends that persist partially or out of order. The method is to record the file operations, then build every crash state the filesystem allows. Our shim (M2) applies the same rules in process, with a seeded choice of crash state.
- **RocksDB `FaultInjectionTestFS`.** Tracks, per file, how much has been synced and, per directory, which entries are new since its last sync. On a "crash" it drops unsynced data (all of it, or a random part) and the unsynced entries. It also injects read and write errors. This is the model for our `LossyFs` (M2).
- **Jepsen LazyFS.** A FUSE filesystem with its own page cache. It can clear the cache, tear one write, or keep only some writes of a sequence, on command through a FIFO. It is a research prototype with no releases (MIT), and it needs libfuse3 and `/dev/fuse`. Because it runs the real binary on a real path, it is our check that the shim models a real filesystem (D-R-5).
- **TigerBeetle VOPR, FoundationDB simulation.** Deterministic simulation of the whole system with injected storage faults (latent sector errors, misdirected and torn writes, slow disks). It needs every I/O, clock and random source behind a seam. Out of scope for R (§ Later: deterministic simulation), but the M2 seam is the first step.
- **GreptimeDB** runs Chaos Mesh pod kills and a fuzz-test crate in CI. **Lance** has its own throttling `ObjectStore` wrapper and fault-injection tests for retries. **arrow-rs** keeps fixing decoder panics found by fuzzers (e.g. arrow-rs#8602, #9868). Malformed Parquet can still panic `parquet`, so we fuzz the path as we use it, behind our own error boundary (M3).
- **`object_store` crate.** `ThrottledStore` adds per-call latency only. The AWS client retries 5xx (503 included), 429, 408, connection errors and resets, up to 10 times within 3 minutes. It retries timeouts only for requests marked idempotent. Toxiproxy works at the TCP level, so it can reset or stall a connection but cannot return an HTTP 503 (M4).
- **S3 multipart.** An incomplete multipart upload is kept, and billed, until it is aborted or a lifecycle rule removes it. Hetzner documents such a rule. Our flush and compaction must abort what they start, and the self-hosting docs must recommend the rule (M4).

## Other tracks in `crates/storage`

Two other tracks change the same crate while R runs (cross-track review X20):

- **N's M2, online backups** (`docs/plans/n-packaging/m2/`, B1–B5 and B9):
  - `gc.grace` up to 30 days;
  - `Storage::flush_now`;
  - an `object_gc_gate` and a pinned catalog snapshot (`snapshot_catalog`);
  - `Storage::copy_objects`, which hard-links objects into a backup;
  - `Storage::collect_orphans`, which lists the object store;
  - pre-migration copies.
- **P's M4, the data-access window** (D-P-21): `QueryOptions::visible_from` on every provider.

How R handles them:
- **Rebase points.** M1's code is test-only (`src/model_tests/`), so it conflicts with neither, except on the `mod` lines of `lib.rs`. M2 (the `fs` seam) rebases onto whatever of B3–B5 and B9 has merged when M2 starts. Its inventory of file operations then includes `copy_objects`' hard links and the pre-migration copies, and the § Durability table covers them.
- **Model coverage.**
  - Once B2 merges, the M1 model gets a `Flush` op through `flush_now`, a flush without a restart.
  - Once P's M4 merges, a follow-up sub-phase adds `visible_from` to the model's reads: spans and traces hidden by their own `start_time` (D-P-21's straddling-trace rule stays P's design).
  - M6's targeted race tests cover the `object_gc_gate` against GC and the snapshot against concurrent commits.
- **Orphans.** M4's orphan audit is `collect_orphans` in dry-run mode (§ M4).
- **Dependencies.** R's roadmap row lists N's M2 and P's M4 as ordering constraints for M2, M4 and the `visible_from` follow-up.

## Milestones

The order puts first what the user called most important: wrong results (D-R-1). Each milestone ends with its tests in `just test` or a documented `just` recipe, the bugs they found fixed (D-R-2), and a findings section in `docs/research/storage/hardening.md`.

### M1 — Model-based correctness

A proptest state machine runs against a simple in-memory model of the reads behind every page, plus the scope reads of deletions and the alert measures. It replaces `compaction/model.rs` (D-R-3).
- **Operations:** appends of whole traces and of late spans (with re-sends), score writes, updates and deletes, trace and user deletions, retention, compaction passes, GC, deletion passes, clean restarts, and **crashes without shutdown** (D-R-4). All of these run across two projects.
- **Checked reads:** through the public `reads` API on a `QueryContext`, the same functions the server calls:
  - `trace_list` (unwindowed and windowed), `trace_detail`, `span_list`, `span_detail`;
  - `score_list`, `latest_scores`, `scores_by_id`;
  - `session_list`, `user_list`;
  - the `metrics` KPIs;
  - `deletion_status`, and the project isolation between the two projects.
- **Two kinds of check:**
  - Bounds, while duplicate re-sends are not yet merged.
  - Exact equality once the state is *settled*: everything flushed, deletions applied, every partition fully compacted.
  - After settling, the files are also checked physically: no covered row is left, and there is one partial per trace.
- **Budget:** up to 16 cases of up to 24 ops in `just test`, kept under a minute in debug. `LLMOBS_MODEL_CASES` sets the count for long runs, which go nightly from M3 (D-R-14).

Plan: `01-m1-model.md`.

### M2 — Durability: losing unsynced writes

- **An `fs` seam.** Every local file operation of `crates/storage` (`wal/`, `objects.rs`, `catalog` paths, `read_cache.rs`, `disk.rs`, startup cleanup) goes through one module, `src/fs.rs`. This is a concrete type, not a trait: in production it forwards to `std::fs`; under `cfg(test)` it can be switched into **lossy mode** by the test that owns the data directory. Lossy mode records, per file, the synced length and the unsynced writes, and per directory the unsynced creates, renames and unlinks. A *lossy crash* then applies, with a seeded RNG, one of these:
  - drop everything unsynced;
  - drop a random subset in an order the filesystem allows;
  - tear the last write.

  SQLite (`catalog.db`, `meta.db`) does its own I/O, so the shim cannot see it. Its durability is SQLite's job (`synchronous=FULL`, D83), and LazyFS covers it.
- **Lossy crashes in the M1 model**, as a new operation (only acknowledged data must survive), and in the hook-abort children. The lossy state lives in the process that wrote, so **the child applies the seeded lossy crash to the directory before it calls `process::abort`**; the parent has no record of what was unsynced.
- **Crash at a hook in the model test** (full-plan review R-1). The child re-executes a case's op list, arms `abort_at(point)` on the last op (a compaction, a deletion pass, a flush through restart, GC, retention or drop), optionally applies a lossy crash, and aborts. The parent applies the same ops to the oracle, reopens the directory and runs `check_full`. For this, op results must be reproducible: deletion ids are derived from (case, op index) instead of `Uuid::now_v7`.
- **The lossy shim serves only in-crate tests.** It exists under `cfg(test)`, and `tests/crash.rs` is an integration test linked without it. The real-binary path, `tests/crash.rs` included, is covered by LazyFS.
- **Confirm and fix** the audit findings above. **Every fsync error is fatal** (D-R-6):
  - the WAL as today;
  - a local object publish fails the job and marks the store unhealthy, with no retry on the same file;
  - a catalog commit that fails with an I/O error stops the committer and reports the storage unhealthy, until a restart.
- **LazyFS cross-check** (`tools/lazyfs/`): a script that mounts LazyFS over a scratch directory and runs the existing crash children and a short ingest against it, sending `clear-cache` at hook points. It runs by hand and nightly. A run that LazyFS fails but the shim passes is a shim bug. Findings and the comparison go in `hardening.md`.
- **A durability audit table** in spec 03 (new § Durability): every write path, what it syncs and in what order, and which test covers it.

### M3 — Fuzzing

- **Layout.** `fuzz/` at the repository root is a cargo-fuzz crate with its own empty `[workspace]`, outside the main workspace. It needs nightly Rust and libFuzzer, which `just check` must not depend on (D-R-7). It still goes through the repository's supply-chain checks (cross-track review X23):
  - `fuzz/Cargo.lock` is generated from the root lock by `just fuzz-lock`, not committed, so it never drifts from the shipped versions;
  - `just deny` also runs `cargo deny --manifest-path fuzz/Cargo.toml check` with the same `deny.toml` (plus a narrow NCSA licence exception for libfuzzer-sys);
  - `.github/dependabot.yml` gets a `/fuzz` cargo entry limited to `libfuzzer-sys`;
  - AGENTS.md § Dependencies records the exception: `fuzz/` pins its one fuzz-only crate (`libfuzzer-sys`) in its own manifest; `arbitrary` comes from the root `[workspace.dependencies]`, and everything else through path dependencies on the workspace crates.
- **Entry points.** Each target calls one function of the crate under test. That function is `pub` and `#[doc(hidden)]` under `cfg(any(test, fuzzing))` (cargo-fuzz sets `--cfg fuzzing`), so the decoders stay crate-private for everyone else.
- **Targets:**
  - `storage`:
    - `wal_record` (one record decode);
    - `wal_replay` (a segment directory: bytes split into segments, replayed into a memtable);
    - `parquet_file` (open and read a whole file through our provider path, with footer and page index);
    - `read_cache_block` (block, CRC and the `size` file);
    - `catalog_rows` (`FileMeta` and deletion rows as loaded at open, from arbitrary column values);
    - `filter_dsl` (A2's filter parser).
  - `otel`:
    - `otlp_http_protobuf`, `otlp_http_json` (decode and normalize);
    - `otlp_grpc` (decode only).

  Structured inputs use `arbitrary` where raw bytes would rarely reach deep code, for example a valid WAL header around a fuzzed Arrow IPC payload.
- **Corpus.** `fuzz/corpus/<target>/` is committed: minimized (`cargo fuzz cmin`), at most about 2 MiB per target, seeded from real files (WAL segments and Parquet files written by the tests, `fixtures/otlp/`). Crashes become `fuzz/regressions/<target>/<name>`, and each one gets a fix plus a named unit test.
- **CI** (user): a `#[test]` per target replays the corpus and regressions on stable in `just test`, in seconds. A nightly workflow (`.github/workflows/nightly.yml`, outside `ci-ok`) installs nightly Rust, fuzzes each target for 10 minutes, uploads new corpus entries as an artifact, and opens an issue on a crash. The same workflow runs the long M1 model run, M6's stress runs (30 minutes local, 10 minutes remote), and the LazyFS check if M2's probe shows `/dev/fuse` works on runners. M5's disk-full tests do not run nightly.
- **Panic boundary.** A panic in the Parquet or Arrow decoder of stored bytes is caught at our scan and treated as a decode error, so the files are verified and quarantined (D-A1-10, D-R-17). Today such a panic drops the request, kills the compaction task, or prevents startup.

### M4 — S3 fault injection

- **`ChaosStore`** (test-only): a wrapper of `Arc<dyn ObjectStore>` implementing `object_store`'s own trait, as `CacheStore` and `FaultStore` already do. It replaces `FaultStore`. A seeded schedule injects:
  - latency (as `ThrottledStore`);
  - 503 SlowDown and 500 errors before or after the request takes effect (the "ambiguous upload" of spec 03 §2), as the S3 client returns them after its retries;
  - multipart failures (one part fails, `complete` fails, `complete` succeeds but reports an error, `abort` fails);
  - `head` and `get` errors, and bodies cut mid-stream. Wrong bytes appear only in focused tests: over TLS they are not a realistic S3 answer, and the model could not tell them from right ones;
  - no listing faults: storage never lists the bucket, and the model asserts it.
- **Over the network,** toxiproxy in front of Versity Gateway adds connection resets, stalls and bandwidth limits, so that `object_store`'s HTTP retry path runs for real. This is an ignored test with a `just` recipe.
- **Checks:**
  - no acknowledged data lost;
  - no wrong result (the M1 model gets an S3 mode with `ChaosStore`);
  - every staged object either becomes a file or is collected;
  - every multipart upload we start is completed or aborted, except those only a lifecycle rule can remove (a failed abort, a lost create answer, a cancelled task);
  - an orphan audit is clean after GC. It is N's `Storage::collect_orphans` in dry-run mode (N M2, B5), not a second lister. If R's M4 lands before B5, M4 writes the audit as a test helper, and B5 replaces it.
- **Real S3.** The `tests/s3.rs` suite and the M1 model's S3 mode run once against a Hetzner Object Storage bucket the user provides when M4 starts (credentials in environment variables only). The run also probes conditional puts, list-after-write, multipart limits and the abort lifecycle rule. Results go in `hardening.md`; `docs/self-hosting/` recommends the lifecycle rule.

### M5 — Resource exhaustion

- **Full disk.** Ignored tests run on a small dedicated filesystem: a size-limited tmpfs mounted inside an unprivileged user and mount namespace (`unshare -rm`), or a loop device the operator provides through `LLMOBS_TEST_SMALL_FS`. The disk fills during WAL writes, flush, compaction, query spill and read-cache fill. Each case checks a clean refusal (`DiskFull`, `WalFailed`, deferred compaction, a query failing as too large), recovery once space is freed, and the M1 model's reads afterwards.
- **Spill capped.** Query spill is checked against `storage.min_free_bytes`, through DataFusion's disk manager limit (`benchmarks.md` § Findings (a)). This is a fix with a spec 03 §6 update.
- **Memory limit.** Runs under a cgroup (`systemd-run --user --scope -p MemoryMax=…`) at the documented minimum. They are storage-level tests: the expected outcomes are `AppendError::Backpressure`/`DiskFull`, `QueryError::Busy` and `RunError::TooLarge` (which the server maps to 503 and 422), never an OOM kill, and that minimum gets documented (full-plan review R-5).
- **File descriptors.** Runs under a low `RLIMIT_NOFILE`, set in a re-executed child as the crash tests do: many partitions, compaction and queries at once. The result must be errors, never a panic or a wedged writer. The descriptors the engine needs at a given scale get documented.

### M6 — Concurrency

Not loom, not shuttle (D-R-10). The model test's *concurrent mode* runs the background tasks for real:
- short flush, compaction and GC intervals;
- queries running on other tasks while the ops proceed.

Every concurrent read must equal the model **before or after** each op in flight. That works because reads take one published `ReadState`, so a snapshot is atomic (spec 03 §4). Stress runs in release mode repeat this for minutes with many threads. A race the model finds but cannot shrink to a reproducible case is the trigger to revisit shuttle for that one component.

### M7 — Scale and soak

- **Realistic payloads.** A payload generator built from the structure of the captured `fixtures/otlp/` cases (messages, tool calls, JSON arguments), with text from a permissively licensed corpus chosen in M7's plan. Compression ratios then match production. The generator is used by the benchmarks and the soak.
- **The pending 5M-trace `trace_bench` run,** locally and against the Hetzner bucket.
- **Soak** (D-R-11): `crates/storage/tests/soak.rs` (ignored, release mode, a D-A1-12 style benchmark) drives the storage directly at a steady ingest rate, with score writes, deletions, retention and queries. Background compaction stays on. Every minute it samples RSS, open file descriptors, file count per level, compaction lag (age of the oldest L0 file), WAL bytes, memtable bytes, the read cache, and query latency percentiles. It fails on monotonic growth past set bounds. It is an integration test, so it sees only the public API (D-A1-12): its correctness check is its own registry of sampled traces read back through `trace_detail`, not the `cfg(test)` M1 oracle, and file counts, compaction lag and WAL bytes come from reading `catalog.db` and the WAL directory directly, as `tests/crash.rs` does, plus the public `health()` and `read_cache_stats()` (full-plan review R-5). A new public stats API would be a coordinator request.
  - A 24 h dry run on the workstation comes first.
  - Then 72 h on a Hetzner server with NVMe, in the bucket's region (user).
  - The write-up, `docs/research/storage/soak.md`, has commands, machine, plots and raw samples.

### M8 — Format stability

The rule, written into spec 03 (new § 12 Format versions), D-R-12 and D-R-26 to D-R-32:
- **Format surfaces:** the WAL segment header and record format; the Parquet schemas and KV metadata; the `catalog.db` schema and its data-directory marker; the object layout and paths; the read-cache layout (pinned, not versioned).
- **One `FORMAT_VERSION`** for the whole directory, stored in `catalog.db`'s `user_version`, in every WAL segment header (new) and in every Parquet file as `llmobs.format_version` (new; `llmobs.schema_version` stays the row normalization version).
- **Not a format change:** adding a nullable Parquet column (spec 03 §3 Schema evolution) and a new `catalog.db` migration that only adds. Anything else is a format change. Examples: a new WAL `kind`, a changed sort order or column type, a renamed path, a changed checksum, a new value of a catalog TEXT enum.
- **Before the first public release:** no constraint (no users, `AGENTS.md`). Directories written before M8 are refused, not migrated; that includes stored benchmark and soak datasets (`trace_bench`'s, M7's), which must be regenerated.
- **From the first public release:** a format change needs a `FORMAT_VERSION` bump, readers or an open-time migration for what the previous release wrote, tests against the previous releases' golden directories, and an `### Upgrade notes` line (D-N-5).
- **Downgrade:** a newer directory, a newer WAL segment or an unknown catalog migration is refused before anything is changed (an older binary used to truncate a newer WAL tail and quarantine a newer Parquet file).
- **Golden directories:** `just golden-dir <version>` writes a small data directory (three projects, one dropped; every table; an applied and a pending deletion; a retention watermark; L0 and L1 files; an obsolete file; a media blob; WAL tail records; a cache entry) as a plain tree `fixtures/storage/golden/<version>/` with `expected.json`, at most 1 MiB. None is committed before the first stable tag; until then the test generates one into a temp dir and checks it. A test opens a copy of each with the current code, compares the public reads with `expected.json`, then writes and compacts on it. The reads are the server's `reads` functions called directly, not the M1 oracle's wrappers.
- **Which directories CI keeps (user):** the 0.x directories until 1.0. 1.0 may drop them, with an upgrade note to pass through the last 0.x first. From 1.0, every release's directory stays. `fixtures/storage/golden/RELEASES` lists them and `just check-golden` guards deletions.
- **Release step.** Producing `fixtures/storage/golden/<version>/` is a step of N's release process: `release-prep` runs `just golden-dir`, and `release-verify` checks that the directory exists. M8 asks the coordinating session to add it to N's scripts (cross-track review X21).

Plan: `08-m8-format.md`. The first-release trigger is settled by D185 (the first public release tag); M8 lands before it.

## Cross-cutting rules

- **Scope of edits.**
  - `crates/storage/`, `crates/otel/` (fuzz entry points and fixes only), `fuzz/`, `tools/lazyfs/`, `justfile`, `docs/`;
  - `AGENTS.md`, for its Commands, Tests, Layout and CI sections, whenever R adds a recipe, an environment variable, a directory or a workflow;
  - `.github/workflows/nightly.yml`, `.github/dependabot.yml`, `deny.toml` and the `just deny` recipe (M3, for `fuzz/`);
  - the root `Cargo.toml`'s `[workspace.lints.rust]` table, for the `unexpected_cfgs` check-cfg entry `cfg(fuzzing)` (M3, full-plan review R-4), and `[workspace.dependencies]` for new dependencies;
  - `CHANGELOG.md`, for every user-visible fix (AGENTS.md § Commits);
  - `tools/soak/` (M7).

  Changes to `llmobs-core`, to the server or to N's release scripts (M8's golden-directory step) are requested from the coordinating session.
- **Fixes** (D-R-2). A bug found by an R test is fixed in R, in a sub-phase of its own, with the shrunk case as a named regression test. The fix and the test share a commit. A fix that needs a format, public API or spec semantic change stops and is raised first.
- **No production cost.** Test seams compile to nothing outside `cfg(test)`/`cfg(fuzzing)`, as `hooks.rs` does. The `fs` module's production path is a direct call.
- **Findings log.** `docs/research/storage/hardening.md` records every bug found: milestone, test, shrunk case, cause, fix commit. Each run has its machine and command. This is the public record behind the robustness claims.

## Later: deterministic simulation

Full deterministic simulation (madsim, turmoil, a TigerBeetle-style simulator) is out of scope. It would need:
- every file and object store call behind one seam (M2's `fs` module and the `Arc<dyn ObjectStore>` the storage already takes);
- every clock read through one place (`catalog::now_ns()` and `tokio::time` today; `Instant::now` and `SystemTime::now` are also called directly);
- every random choice from a seeded source (UUIDv7 names);
- no thread spawned outside the runtime (`spawn_blocking` for the WAL and Parquet writers).

R moves toward this without building it. M2 adds the `fs` seam, and new code takes `now` as a parameter where the existing API already does (`compact(now)`, `apply_deletions(now)`). The remaining gaps (direct clock calls, `spawn_blocking`, the SQLite VFS) are listed in `hardening.md` at the end of M2.

## Open questions

- **M7 text corpus:** which permissively licensed corpus supplies the prompt text. Decided in M7's plan.
- **M4 bucket:** the user provides it when M4 starts (region, bucket; keys in env only).
- **LazyFS in GitHub Actions:** whether `/dev/fuse` works on `ubuntu-24.04` runners is unverified. If it doesn't, the LazyFS check runs on the soak machine instead. Settled in M2's plan.
