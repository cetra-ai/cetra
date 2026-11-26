# Storage soak

The soak drives `Storage` at a steady rate for hours. Ingest, score writes, deletions, retention and queries run at the same time, and background compaction stays on. Every minute it records one JSON line of resource and lag samples. At the end it applies pass/fail rules and checks a seeded sample of traces against a registry of what storage must show. The decision is D-R-11 and the plan is `docs/plans/r-hardening/07-m7-scale-soak.md`. Runs are made by hand. The dry run was a 48 h run on a VM (§ Runs); the 72 h run on a dedicated Hetzner AX52 is not made yet.

## Method

- **Workload.** A rate of 2,000 spans/s by default (`CETRA_SOAK_RATE`) across 4 projects with shares 8:4:2:1. Traces are chat, RAG or agent shapes, from 16 apps. LLM spans carry 10–50 KiB of text, other spans under 1 KiB (spec 03's capacity check). The text is oasst2 (Apache-2.0, `just fetch-corpus`, SHA-256 `a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751`). Judge scores are written for 20% of completed traces, with new versions and tombstones of recent scores. Deletions register `Traces` every 10 minutes and `User` every hour. Retention runs hourly for every project, with the boundary `keep_from = (now − 4 h).date − retention_days` in UTC (default `retention_days` 0, so only today is kept, and yesterday until 04:00 UTC). The 4 h lag lets a day close (spec 03 §5) before retention drops it. Queries run at 1 per second (`CETRA_SOAK_QPS`, 0 for none), from one task by default (`CETRA_SOAK_QUERY_TASKS` tasks share the rate; one task runs its queries one after another) in the mix list 30, filter 15, detail 25, sessions 10, metrics 10, scores 10.
- **Pacing.** Each project's ingest task ticks every 100 ms and sends the spans its share of the rate allows for the time since its last tick, so a tick that fires late (a slow append, a busy runtime) sends a larger batch instead of losing its share: `ingest_rate` measures whether storage keeps up with the rate, not the driver's jitter. Runs before `20261125` paced per tick, and a late tick lost its spans (§ 20261124). Each sample records the span appends' latency (p50, p99, max, backpressure waits included) and the ticks' lateness (p99, max) in `appends`.
- **Allocator.** The test binary uses mimalloc, the `cetra` binary's allocator (D56), so the RSS is the one the server would have. Runs before `20261125` used glibc malloc.
- **Storage config.** The production defaults of spec 03, plus the data directory, the S3 target (when set), WAL sync `always`, `min_free_bytes` 2 GiB, compaction every 30 s and a read cache of 10% of the filesystem. `GcConfig` has no interval, so GC keeps its default cadence.
- **Samples.** One line per `CETRA_SOAK_SAMPLE_SECS` (default 60 s) in `samples.jsonl`; the fields are the `Sample` struct in `crates/storage/tests/it/soak/sample.rs`. Process values come from `/proc/self`. Ingest values are cumulative counters. Storage values come from `health()`, `read_cache_stats()` and a read-only catalog transaction. Orphans are confirmed only when a path is a suspect in two samples and is older than 10 minutes. Query latencies cover the interval since the last sample. No field holds a path, a host or a bucket. A sample that cannot be read is retried once; a second failure is a `Sample` failure and the run goes on.
- **Machine.** The machine line in `run.json` describes the disk under the data directory. In S3 runs the objects are in the bucket, so that disk is the local one, not the storage's object store.
- **End of run.** Stop the drivers (pending late groups are appended). Take a sample. Shut down, drop and reopen the storage, timing both. Run the oracle at full size. Settle for at most three rounds: wait for the timed flush, then compact and apply deletions at now + 3 days, then GC. Take a settled sample, run the oracle again, then shut down. The S3 prefix of a passing run is deleted unless `CETRA_SOAK_KEEP=1`.
- **Redaction.** `run.json` and `summary.json` are redacted before they are written. The bucket, the S3 endpoint host, `$HOME` and the hostname become placeholders. `events.jsonl` is scrubbed into `events.scrubbed.jsonl`, and only the scrubbed file is committed.

## Rules

Growth rules are enforced only when `hours ≥ max(warmup_hours, (retention_days + 1) × 24) + 24`. Enforced growth reads from `max(warmup_hours, (retention_days + 1) × 24 h)`; shorter runs read from `warmup_hours` and report `Info`. Samples after the run's end (`hours` plus one sample interval, the stop and the settle) are not growth, `compaction_lag` or `rss_max` evidence. `compaction_lag` always reads from `warmup_hours`. `query_growth_<kind>` skips samples with no queries of that kind. Harness rules are always enforced. `Skipped` and `Info` rules never count as a pass.

| Rule | Check | Bound | Kind |
|---|---|---|---|
| `rss_growth` | `grows(rss_bytes)` | rel 0.10, abs 256 MiB | growth |
| `fd_growth` | `grows(fds)` | rel 0.10, abs 64 | growth |
| `thread_growth` | `grows(threads)` | rel 0.10, abs 8 | growth |
| `wal_growth` | `grows(wal_bytes)` | rel 0.5, abs 2 × `wal.segment_bytes` | growth |
| `memtable_growth` | `grows(memtable_bytes)` | rel 0.5, abs `flush.max_bytes` | growth |
| `l0_growth` | `grows(files.spans.l0)` | rel 0.5, abs `compaction.l0_files` | growth |
| `query_growth_<kind>` | `grows(p95_ms)` per kind | rel 0.5, abs 50 ms | growth |
| `rss_max` | max `rss_bytes` | ≤ `max_rss_gib` (8) | harness |
| `ingest_rate` | Δappended / Δtarget of first-group spans, 1 h windows from 15 min (10 min windows for runs under 2 h) | ≥ 0.99 | harness |
| `compaction_lag` | max `l0_oldest_age_s` after warm-up | ≤ `max_l0_age_min` (30) | harness |
| `day_close` | `overdue_partitions` | 0 at every sample | harness |
| `retention` | `retention_violations` | 0 at every sample | harness |
| `deletion_lag` | max `oldest_pending_deletion_s`; every deletion applied by the end | ≤ 24 h | harness |
| `orphans` | confirmed `orphans` | 0 at every sample | harness |
| `wal_failed` | any sample | never | harness |
| `sampling` | samples that could not be taken (after a retry) | 0 | harness |
| `appends`, `queries`, `deletions`, `retention_calls` | failure counts | 0 | harness |
| `log_errors` | ERROR lines in `events.jsonl` | 0 | harness |
| `oracle` | oracle mismatches | 0 | harness |
| `settled` | the settled-state conditions | all hold | harness |

## Runs

| id | date | commit | machine | store | hours | rate | result | data |
|---|---|---|---|---|---|---|---|---|
| `20261122-vm-48h` | 2026-11-22 | 041b2b1 + #214 | VM, 12 vCPUs, virtual disk | local | 48 | 1,000 | fail: `rss_growth`, `rss_max`, `ingest_rate` | [`soak/20261122-vm-48h/`](soak/20261122-vm-48h/) |
| `20261124-vm-noq-12h` | 2026-11-24 | b0e5710 | VM, 12 vCPUs, virtual disk | local | 12 | 1,000, no queries | fail: `rss_max`, `ingest_rate` (0.9885 at worst) | [`soak/20261124-vm-noq-12h/`](soak/20261124-vm-noq-12h/) |
| `20261124-vm-q-12h` | 2026-11-24 | b0e5710 | VM, 12 vCPUs, virtual disk | local | 12 | 1,000, 1 query/s | fail: `rss_max`, `ingest_rate` | [`soak/20261124-vm-q-12h/`](soak/20261124-vm-q-12h/) |
| `20261125-vm-q4-12h` | 2026-11-25 | 8487c09 | VM, 12 vCPUs, virtual disk | local | 12 | 1,000, 4 queries/s from 4 tasks | pass (28 rules; growth rules reported, not enforced) | [`soak/20261125-vm-q4-12h/`](soak/20261125-vm-q4-12h/) |
| `20261125-vm-q4-qrt-12h` | 2026-11-25 | b99b20a | VM, 12 vCPUs, virtual disk | local | 12 | 1,000, 4 queries/s from 4 tasks, query runtime (D-R-39) | pass (28 rules; growth rules reported, not enforced) | [`soak/20261125-vm-q4-qrt-12h/`](soak/20261125-vm-q4-qrt-12h/) |

The VM run is a dry run on a virtual disk, on a shared VM: its numbers are not publishable bare-metal numbers and back no claim.

### The VM dry run: rate and calibration

The dry run runs on ubuntu-vm: a VM with 12 vCPUs, 94 GB of RAM and a 500 GB virtual disk whose file lives on another server's NVMe. It is a dry run on a virtual disk, on a VM other work shares, not publishable bare-metal numbers. It runs 48 h, so that the growth rules are enforced with `retention_days` 0 (§ Rules), in local mode.

The disk budget for the run was about 150 GB at peak. With `retention_days` 0, a run keeps up to 28 h of data (yesterday until 04:00 UTC, then today), so the default 2,000 spans/s did not fit:
- **Calibration.** One hour at 2,000 spans/s (`CETRA_SOAK_HOURS=1`, label `cal`), and the first 9 h of an earlier run at 400 spans/s that was stopped when the plan changed. Both stored about 1.15 KB per span on disk (14.6 GB of objects for 12.9 M spans after 9 h; 8.7 GB for 7.2 M spans after 1 h, counting files GC had not collected yet). The generator's payload averages 4.6 KB per span, so the stored data is about 4× smaller.
- **Rate.** 28 h at R spans/s takes about R × 116 MB, plus 15% for compaction outputs and files waiting for GC. The WAL stays under 0.1 GB, and local mode has no read cache. `CETRA_SOAK_RATE=1000` gives about 116 GB of data and 135 GB at peak, under the budget, and leaves room under 2,000.
- **What the calibration found.** `settled` failed on a day close that could never fit the compaction pool (M7-1 in `hardening.md`, fixed before the 48 h run). `rss_max` failed (8.49 GiB, M7-2) and `ingest_rate` failed (0.973 of 2,000 spans/s in the last window, M7-3). The oracle, orphans, retention, deletions and `log_errors` passed. **RSS grows; the 48 h run confirmed it (`rss_growth` fails, § `20261122-vm-48h`).** In the stopped 400 spans/s run, RSS went from 6.4 GB at 1 h to 9.8 GB at 3 h and 10.9 GB at 6 h, then held at 10.9 to 11.0 GB until 9 h, with the memtable at 0.25 GB and no read cache (M7-2, M5-7). The stopped 400 spans/s run crossed 04:00 UTC and dropped the previous day on time (24 → 12 partitions), with no failure, orphan or retention violation in 9 h.

## 20261122-vm-48h: 48 h dry run at 1,000 spans/s on a VM

- **Machine.** `Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz`, 12 threads, 94 GiB RAM, kernel 7.0.0-43-generic, disk `vda rota=1` (a virtio disk whose image file is on another server's NVMe), ext4 `rw,relatime,discard,errors=remount-ro,commit=30`, no container. Other work shared the VM's host during the run.
- **Code.** A `git archive` of 041b2b1 with the M7-1 fix and its log field applied (the two commits of #214) and the D3 test fix (M5-9). `run.json` has an empty `commit` because the copy is not a git checkout.
- **Command.** In a `systemd-run --user` unit with `MemoryMax=16G` and `LimitNOFILE=65536`: `CETRA_SOAK_HOURS=48 CETRA_SOAK_RATE=1000 CETRA_SOAK_RETENTION_DAYS=0 CETRA_SOAK_LABEL=vm CETRA_SOAK_DIR=… CETRA_SOAK_OUT=… just soak`. Started 2026-11-22 10:12:53 UTC, ended 2026-11-24 10:23:34 UTC (173,441 s, settle included).
- **Result.** 28 rules, 25 pass, 3 fail. The growth rules are enforced: the run is 48 h, `max(warmup_hours, (retention_days + 1) × 24) + 24` with `warmup_hours` 4.8.

| rule | verdict | detail |
|---|---|---|
| `rss_growth` | **fail** | p95 per quarter 12.61, 13.22, 15.02, 15.21 GB |
| `rss_max` | **fail** | peak RSS 14.47 GiB above 8 GiB |
| `ingest_rate` | **fail** | every 1 h window below 0.99: 0.980 in the first, falling to 0.926 at 18 h, 0.965 after the first retention drop, 0.904 at 41 h, 0.955 after the second drop, 0.945 in the last |
| `fd_growth`, `thread_growth`, `wal_growth`, `memtable_growth`, `l0_growth` | pass | |
| `query_growth_list`, `_filter`, `_detail`, `_sessions`, `_metrics`, `_scores` | pass | |
| `compaction_lag`, `day_close`, `retention`, `deletion_lag`, `orphans`, `wal_failed`, `sampling` | pass | |
| `appends`, `queries`, `deletions`, `retention_calls`, `log_errors`, `oracle`, `settled` | pass | |

- **Key numbers.** From `just soak-plot` and `summary.json`:

| series | min | median | p95 | max |
|---|---:|---:|---:|---:|
| RSS, GiB | 8.576 | 11.65 | 14.01 | 14.47 |
| fds | 20 | 20 | 24 | 72 |
| threads | 21 | 46 | 54 | 54 |
| WAL, MiB | 1.368 | 121.5 | 176.5 | 200.1 |
| memtable, MiB | 0 | 137.5 | 251.1 | 314 |
| spans L0 files | 0 | 16 | 37 | 72 |
| oldest L0 file, s | 0 | 193.1 | 391.6 | 787.7 |
| objects, GiB | 13.74 | 39.57 | 80.67 | 107.8 |

  - Ingest: 936 spans/s on average over the run against 1,000, at 4.6 KB of payload per span (764 GB generated). No backpressure: the wait counter stayed at 0.
  - Process CPU: 1.85 cores on average. Per hour it grew with the retained data, from 1.1 cores at 2 h to 2.2 at 17 h and 2.3 at 41 h, and fell back after each drop.
  - Queries, p95 of the per-minute p95s: list 720 ms, filter 786 ms, detail 173 ms, sessions 615 ms, metrics 3,001 ms (max 5,522 ms), scores 148 ms. 150 k queries, no error.
  - Deletions: 336 registered and applied, none pending at the end. Lag p50 420 s, p95 1,080 s, max 1,681 s.
  - Retention: both daily drops ran (24 → 12 partitions each time). Objects peaked at 107.8 GiB before the second drop; free disk never fell below 188 GB.
  - End of run: shutdown 2.8 s, reopen 0.29 s, settled in 3 compaction passes. The oracle passed at full size and after the settle.
- **Plots.**

![memory](soak/20261122-vm-48h/plots/memory.png)
![ingest](soak/20261122-vm-48h/plots/ingest.png)
![lag](soak/20261122-vm-48h/plots/lag.png)
![files](soak/20261122-vm-48h/plots/files.png)
![wal and memtable](soak/20261122-vm-48h/plots/wal_memtable.png)
![queries](soak/20261122-vm-48h/plots/queries.png)
![disk](soak/20261122-vm-48h/plots/disk.png)
![fds and threads](soak/20261122-vm-48h/plots/fds_threads.png)
![cache](soak/20261122-vm-48h/plots/cache.png)

- **What it shows.**
  - **RSS grows over two days (M7-2).** It goes up in steps, at about 10 h, 12.5 h, 16 h and 36 h, and does not come back down when retention halves the data. The steps at 12.5 h and 36 h both fall around 22:30 UTC, when the day's partitions are near their largest. The memtable stays under 314 MiB, the WAL under 200 MiB, and local mode has no read cache, so the growth is outside the storage's own budgets. Not located yet.
  - **Ingest falls behind as the retained data grows (M7-4).** The hourly ratio follows the objects on disk: it falls through each day, jumps back after each retention drop, and falls again. CPU and the metrics query's p95 follow the same saw-tooth. There is no backpressure and fsync is not slow, so the driver itself does not get enough time. Queries run on the same 8-worker runtime as ingest, which suggests query load (scans grow with the retained data) takes time from ingest. The 12 h runs with and without queries confirmed it (§ 20261124).
  - Compaction, day closes, retention, deletions, orphans, the oracle and the settle all held for 48 h with realistic payloads, after the M7-1 fix.
- **Caveats.** A shared VM on a virtual disk, so latencies and the ingest ratio are lower bounds for bare metal. The rate is 1,000 spans/s, half the default, for the disk budget (§ The VM dry run). Local store only.
- **Deviations from the plan.** The rate (1,000 instead of 2,000 spans/s) and the duration (48 h instead of 24 h), set by the coordinating session for the disk budget and the growth rules. `events.jsonl.gz` is the scrubbed log, gzipped (246 KiB). The 53 `compaction job failed` warnings in `summary.json` are all `error = "stopped"`, logged within one second at 10:13:19 when the end of the run stopped compaction mid-rewrite; none happened during the run.

## 20261124: queries on and off, 12 h each

Two 12 h runs side by side, to tell what the 48 h run's ingest lag (M7-4) and RSS growth (M7-2) come from. They differ only in the queries: `20261124-vm-noq-12h` (A) runs none (`CETRA_SOAK_QPS=0`), `20261124-vm-q-12h` (B) runs 1 query/s as the 48 h run did.

- **Machine.** The same VM as `20261122-vm-48h`.
- **Code.** A `git archive` of b0e5710: the 48 h run's code plus the RSS split (`rss_anon_bytes`, `rss_file_bytes` in the samples) and `CETRA_SOAK_QPS=0`.
- **Command.** Two `systemd-run --user` units, each with `MemoryMax=16G` and `LimitNOFILE=65536`, started together on 2026-11-24 at about 11:38 UTC and ended at 23:52 UTC (44,008 s, settle included): `CETRA_SOAK_HOURS=12 CETRA_SOAK_RATE=1000 CETRA_SOAK_RETENTION_DAYS=0 CETRA_SOAK_QPS=0|1 CETRA_SOAK_LABEL=vm-noq|vm-q … just soak`. The growth rules are reported, not enforced, at 12 h.
- **Result.** Both fail `rss_max` and `ingest_rate` only; the other 26 rules pass or are reported. A's `ingest_rate` fails in three windows, at 0.9898, 0.9885 and 0.9885; B's fails in every window after the second hour.

| hour | A ingest | B ingest | A CPU, cores | B CPU, cores | A RSS, GB | B RSS, GB |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.997 | 0.992 | 0.55 | 0.76 | 6.88 | 7.57 |
| 2 | 0.997 | 0.981 | 0.55 | 1.04 | 6.96 | 9.99 |
| 4 | 0.996 | 0.967 | 0.57 | 1.25 | 8.14 | 12.36 |
| 6 | 0.996 | 0.961 | 0.61 | 1.33 | 8.57 | 12.47 |
| 8 | 0.991 | 0.946 | 0.68 | 1.55 | 8.73 | 12.53 |
| 10 | 0.989 | 0.932 | 0.73 | 1.73 | 9.02 | 12.43 |
| 12 | 0.993 | 0.932 | 0.65 | 1.67 | 8.78 | 12.83 |

The ingest ratio is appended / target spans in each hour, CPU the process's per hour, RSS at the end of the hour. In both runs anonymous memory is all but the whole RSS: file-backed memory stays at 0.06 GB (A) and 0.07 GB (B).

| series | A max | B max |
|---|---:|---:|
| RSS, GiB | 8.59 | 12.12 |
| WAL, MiB | 212.5 | 188.5 |
| memtable, MiB | 342.7 | 267.8 |
| spans L0 files | 32 | 36 |
| oldest L0 file, s | 411.8 | 477.4 |
| objects, GiB | 39.32 | 38.64 |

  - B's queries, p95 of the per-minute p95s: list 814 ms, filter 905 ms, detail 134 ms, sessions 654 ms, metrics 1,519 ms (max 2,699 ms), scores 186 ms. 38.7 k queries, no error; 1 query/s was asked, about 0.9/s ran (one task runs them one after another).
  - Deletions: 84 registered and applied in each run. Lag p95 1,020 s in both.
  - End of run: A shut down in 2.4 s, B in 12.6 s; both reopened in under 0.5 s and settled in 3 passes. B's 56 `compaction job failed` warnings are all `error = "stopped"`, at 23:39:09–10 when the end of the run stopped compaction.
- **Plots.**

![A memory](soak/20261124-vm-noq-12h/plots/memory.png)
![B memory](soak/20261124-vm-q-12h/plots/memory.png)
![A ingest](soak/20261124-vm-noq-12h/plots/ingest.png)
![B ingest](soak/20261124-vm-q-12h/plots/ingest.png)
![B queries](soak/20261124-vm-q-12h/plots/queries.png)

- **What it shows.**
  - **The queries cause most of the ingest lag (M7-4).** Without queries, ingest stays at 0.99 or above for 7 h and bottoms at 0.988; with 1 query/s it falls to 0.93 by 10 h, as in the 48 h run (0.944 at 9 h). B's CPU stays under 2 of 12 cores, and neither run hit backpressure, so ingest does not lack CPU: the paced appends wait on something the queries hold or occupy, such as the shared runtime's workers or a lock on the append path. A's small decline (0.997 → 0.988) also follows the retained data, and some of it may be B's load on the shared disk.
  - **RSS is heap, and the queries add about 4 GB of it (M7-2).** File-backed memory is under 0.1 GB, so the growth is allocations, not mapped files or page cache. Without queries RSS climbs 6.9 → 9.0 GB over 12 h; with queries it reaches 12.4 GB by 4 h and then stays between 12.4 and 12.8 GB. B's plateau looks like a high-water mark the allocator keeps (the query pool and its peak), not a leak; A's slow climb is ingest and compaction alone.
- **Two harness flaws, found after these runs and fixed for the runs after them.**
  - **The driver lost the spans of late ticks.** Each ingest tick added 100 ms of the rate to its budget whatever the time since the last tick, and a missed tick was delayed, not caught up. Any delay of an ingest task (an append's latency, a runtime worker busy with a query) was lost for good, so `ingest_rate` measured the driver's jitter as much as storage's throughput. The budget now follows the clock (§ Method, Pacing), and the samples record the append latency and the tick lateness, which tell a slow storage from a busy runtime.
  - **The test binary used glibc malloc, the `cetra` binary mimalloc (D56).** glibc keeps freed memory in per-thread arenas, a known cause of RSS that rises in steps and does not fall, so the RSS of these runs and of the 48 h run is glibc's, not the server's. The test binary now uses mimalloc.
- **Caveats.** A and B ran at the same time on one VM, so each one's ingest and latencies include the other's load on the 12 vCPUs and the disk; the difference between them is what the queries add, not two clean baselines. 12 h runs do not enforce the growth rules.
- **Deviations from the plan.** None beyond the coordinating session's: these A/B runs were added after the 48 h run to locate M7-2 and M7-4.

## 20261125-vm-q4-12h: 12 h with 4 queries/s on the fixed harness

The first run with clock pacing and mimalloc (§ Method), at four times the 48 h run's query load.

- **Machine.** The same VM as `20261122-vm-48h`, alone on it.
- **Code.** A `git archive` of 8487c09.
- **Command.** In a `systemd-run --user` unit with `MemoryMax=16G` and `LimitNOFILE=65536`: `CETRA_SOAK_HOURS=12 CETRA_SOAK_RATE=1000 CETRA_SOAK_QPS=4 CETRA_SOAK_QUERY_TASKS=4 CETRA_SOAK_RETENTION_DAYS=0 CETRA_SOAK_LABEL=vm-q4-fixed … just soak`. Started 2026-11-25 00:15 UTC, ended 12:24 UTC (43,729 s, settle included). A run with the same settings on the old harness was stopped after 40 minutes, when the harness flaws were found; it is not recorded.
- **Result.** Pass: 28 rules, none failed. The growth rules are reported, not enforced, at 12 h; `rss_growth` reports no growth, and every `query_growth_<kind>` reports growth (below).

| hour | ingest | CPU, cores | RSS, GB | append p50, ms | append p99, ms | tick lateness p99, ms |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1.000 | 1.18 | 2.52 | 12.3 | 37.8 | 24.8 |
| 2 | 1.000 | 1.86 | 2.06 | 11.6 | 62.8 | 60.9 |
| 4 | 1.000 | 2.34 | 3.28 | 11.0 | 87.8 | 109.5 |
| 6 | 1.000 | 2.60 | 2.75 | 11.3 | 123.1 | 148.7 |
| 8 | 1.000 | 3.11 | 2.68 | 11.6 | 161.7 | 198.5 |
| 10 | 1.000 | 3.39 | 3.09 | 11.9 | 202.9 | 256.6 |
| 12 | 1.000 | 3.99 | 2.57 | 11.6 | 306.8 | 453.1 |

Per hour: the ingest ratio, the process's CPU, RSS at the end of the hour, and the median over the hour's samples of each sample's append p50, append p99 and tick lateness p99.

| series | min | median | p95 | max |
|---|---:|---:|---:|---:|
| RSS, GiB | 1.019 | 2.429 | 3.239 | 4.883 |
| fds | 20 | 20 | 23 | 26 |
| threads | 21 | 60 | 60 | 60 |
| WAL, MiB | 50.74 | 117.1 | 172.3 | 182.4 |
| memtable, MiB | 0 | 139 | 258.2 | 282.3 |
| spans L0 files | 0 | 16 | 32 | 32 |
| oldest L0 file, s | 0 | 180.2 | 366.6 | 402.4 |
| objects, GiB | 4.217 | 21.78 | 37.74 | 40.52 |

  - Ingest: 988 spans/s over the run, settle included; 1.000 of target in every hour. No backpressure.
  - Queries: 3.9–4.0 per second, 154 k in all, no error. p95 of the per-minute p95s: list 965 ms, filter 997 ms, detail 314 ms, sessions 841 ms, metrics 1,737 ms (max 3,407 ms), scores 353 ms. Each kind's p95 grows with the retained data, about 2.5× (list) to 5.8× (metrics) from the first quarter to the last.
  - Appends: 1.27 M; p50 11.4 ms all run; p99 (p95 of the samples) 347 ms, max 1,615 ms. Ticks: lateness p99 493 ms, max 1,531 ms.
  - Deletions: 84 registered and applied, lag p95 1,020 s. Shutdown 1.7 s, reopen 0.38 s, settled in 3 passes. No warning in the log.
- **Plots.**

![memory](soak/20261125-vm-q4-12h/plots/memory.png)
![appends](soak/20261125-vm-q4-12h/plots/appends.png)
![ingest](soak/20261125-vm-q4-12h/plots/ingest.png)
![queries](soak/20261125-vm-q4-12h/plots/queries.png)
![lag](soak/20261125-vm-q4-12h/plots/lag.png)
![files](soak/20261125-vm-q4-12h/plots/files.png)
![wal and memtable](soak/20261125-vm-q4-12h/plots/wal_memtable.png)
![disk](soak/20261125-vm-q4-12h/plots/disk.png)
![fds and threads](soak/20261125-vm-q4-12h/plots/fds_threads.png)

- **What it shows.**
  - **Storage keeps up with 1,000 spans/s under 4 queries/s (M7-4 was the harness).** With the budget on the clock, every hour reaches 1.000 of target, where the old harness lost 3% by 3 h at a quarter of this query load.
  - **RSS stays under 5 GiB with mimalloc (M7-2 was glibc's).** It moves between 2 and 3.5 GB with spikes to 4–4.9 GB on the hour (the hourly retention and oracle round), returns after each, and its baseline rises slowly with the retained data (about 1.8 → 2.6 GB). The glibc runs held 12–15 GB.
  - **Queries delay ingest as the day grows (M7-5).** The append p50 stays at 11–12 ms, but the append p99 grows from 38 to 307 ms and the ticks' lateness p99 from 25 to 453 ms, in step with the queries' latency. A late tick means the ingest task waited for a runtime worker: the queries' DataFusion work runs on the same 8 Tokio workers and keeps them busy longer as the scans grow. Throughput holds; latency does not.
- **Caveats.** A shared VM on a virtual disk. 12 h does not enforce the growth rules, nor cross a retention drop. The soak's runtime has 8 workers for load, queries and storage alike, like a server's one runtime.

## 20261125-vm-q4-qrt-12h: the same load on the query runtime

`20261125-vm-q4-12h` again, with the query runtime (D-R-39): every query's and every compaction job's DataFusion execution runs on storage's own `cetra-query` runtime (12 workers, the VM's CPU count, at nice 10), and the soak's 8 workers keep the load, ingest and the rest of storage.

- **Machine.** The same VM, alone on it.
- **Code.** A `git archive` of b99b20a (PR #258).
- **Command.** As `20261125-vm-q4-12h`, with `CETRA_SOAK_LABEL=vm-q4-qrt`. Started 2026-11-25 14:06 UTC, ended 2026-11-26 02:15 UTC (43,739 s, settle included). Three minutes in, the process had 29 `cetra-query` threads (12 workers and the runtime's blocking threads), all at nice 10.
- **Result.** Pass: 28 rules, none failed. The growth rules are reported, not enforced, at 12 h.

| hour | ingest | CPU, cores | RSS, GB | append p50, ms | append p99, ms | tick lateness p99, ms |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1.000 | 1.35 | 2.37 | 12.4 | 34.1 | 3.7 |
| 2 | 1.000 | 2.27 | 2.94 | 14.7 | 41.3 | 4.6 |
| 4 | 1.000 | 3.02 | 4.13 | 15.1 | 43.1 | 6.7 |
| 6 | 1.000 | 3.38 | 4.41 | 15.0 | 41.4 | 7.9 |
| 8 | 1.000 | 4.13 | 3.20 | 14.9 | 43.1 | 9.0 |
| 10 | 1.000 | 4.46 | 4.80 | 15.3 | 42.1 | 8.3 |
| 12 | 1.000 | 4.72 | 5.13 | 15.5 | 42.7 | 7.5 |

Columns as in `20261125-vm-q4-12h`.

| series | min | median | p95 | max |
|---|---:|---:|---:|---:|
| RSS, GiB | 1.559 | 3.138 | 4.136 | 5.274 |
| fds | 24 | 24 | 27 | 32 |
| threads | 33 | 88 | 89 | 89 |
| WAL, MiB | 2.364 | 123.4 | 178.3 | 188.1 |
| memtable, MiB | 0 | 138.3 | 253.5 | 272.5 |
| spans L0 files | 0 | 20 | 39 | 56 |
| oldest L0 file, s | 0 | 182.8 | 370.2 | 401.3 |
| objects, GiB | 4.218 | 21.89 | 37.84 | 41.62 |

Query p95 per hour, the median of the per-minute p95s, in ms, `20261125-vm-q4-12h` → this run:

| hour | list | detail | scores | metrics |
|---:|---:|---:|---:|---:|
| 1 | 163 → 194 | 69 → 90 | 72 → 91 | 82 → 110 |
| 2 | 307 → 453 | 74 → 132 | 104 → 166 | 159 → 252 |
| 4 | 333 → 547 | 83 → 139 | 113 → 172 | 339 → 452 |
| 8 | 489 → 589 | 143 → 208 | 169 → 238 | 838 → 868 |
| 12 | 863 → 846 | 261 → 412 | 277 → 382 | 1,620 → 1,369 |

  - Ingest: 988 spans/s over the run, settle included; 1.000 of target in every hour. No backpressure.
  - Queries: 3.9–4.0 per second, 155 k in all, no error. p95 of the per-minute p95s: list 958 ms, filter 968 ms, detail 512 ms, sessions 847 ms, metrics 1,597 ms (max 2,799 ms), scores 533 ms.
  - Appends: 1.39 M; p50 15.0 ms; p99 (p95 of the samples) 59 ms, max 1,177 ms. Ticks: lateness p99 17 ms, max 1,121 ms.
  - Deletions: 84 registered and applied, lag p95 1,260 s. Shutdown 5.2 s, reopen 1.16 s, settled in 3 passes. The 52 `compaction job failed` warnings in `summary.json` are all `error = "stopped"`, logged within one second at 02:07:09 when the end of the run stopped a deletion pass mid-rewrite, as in `20261122-vm-48h`; none happened during the run. `events.jsonl.gz` is the scrubbed log, gzipped; it holds IDs, sizes and counts only.
- **Plots.**

![memory](soak/20261125-vm-q4-qrt-12h/plots/memory.png)
![appends](soak/20261125-vm-q4-qrt-12h/plots/appends.png)
![ingest](soak/20261125-vm-q4-qrt-12h/plots/ingest.png)
![queries](soak/20261125-vm-q4-qrt-12h/plots/queries.png)
![lag](soak/20261125-vm-q4-qrt-12h/plots/lag.png)
![files](soak/20261125-vm-q4-qrt-12h/plots/files.png)
![wal and memtable](soak/20261125-vm-q4-qrt-12h/plots/wal_memtable.png)
![disk](soak/20261125-vm-q4-qrt-12h/plots/disk.png)
![fds and threads](soak/20261125-vm-q4-qrt-12h/plots/fds_threads.png)

- **What it shows.**
  - **Queries no longer delay ingest as the day grows (M7-5 fixed).** The ticks' lateness p99 stays between 4 and 9 ms in every hour, where it grew from 25 to 453 ms on the shared runtime, and the append p99 stays at 34–45 ms, where it grew from 38 to 307 ms. Neither follows the queries' latency any more.
  - **The append p50 is higher, 15 ms instead of 11–12 ms**, from the second hour on and flat. Not investigated: the appends now share the machine's cores with 12 query workers instead of 8 shared workers, and the p50 is still a fifth of the p99 it replaces.
  - **Short queries are slower, long ones as before.** detail and scores lose 20–70 ms at p95 in the early hours and about 100–150 ms at 12 h; list matches by 12 h, and metrics is faster at 12 h (1,369 against 1,620 ms). The run-wide list, filter, sessions and metrics p95s are within 4% or better; detail (512 against 314 ms) and scores (533 against 353 ms) are worse. Each query now crosses runtimes (spawn and a channel of two batches) and runs at nice 10, and the VM's 12 vCPUs are shared with the host; this run does not tell those apart.
  - **The rest holds.** RSS peaks at 5.27 GiB (4.88 before), with the same hourly spikes; 29 more threads; spans L0 peaks at 56 files (32 before; p95 39 against 32), with compaction now at nice 10 too; CPU is 0.2–0.7 cores higher per hour.
  - **Single stalls remain.** The append max (1.2 s) and the tick lateness max (1.1 s) are below the shared runtime's (1.6 s, 1.5 s) but not gone; the hourly maxima do not grow with the data. Their cause (a slow fsync on the virtual disk, a blocking call on the main runtime) is not located.
- **Caveats.** As `20261125-vm-q4-12h`. One run of each, on a shared VM: the query latency differences are within what a busier host could cause, and an A/B at nice 0 would tell the priority's share from the runtime hop's.

## Claims

| claim | evidence (run id, metric, value) | caveats |
|---|---|---|

No claim is made yet. A claim names the run's rate, machine and duration, and rests only on rules that read `Pass`.
