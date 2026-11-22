# Storage soak

The soak drives `Storage` at a steady rate for hours. Ingest, score writes, deletions, retention and queries run at the same time, and background compaction stays on. Every minute it records one JSON line of resource and lag samples. At the end it applies pass/fail rules and checks a seeded sample of traces against a registry of what storage must show. The decision is D-R-11 and the plan is `docs/plans/r-hardening/07-m7-scale-soak.md`. No run is recorded yet: the 24 h dry run and the 72 h run on a dedicated Hetzner AX52 are run by hand.

## Method

- **Workload.** A rate of 2,000 spans/s by default (`CETRA_SOAK_RATE`) across 4 projects with shares 8:4:2:1. Traces are chat, RAG or agent shapes, from 16 apps. LLM spans carry 10–50 KiB of text, other spans under 1 KiB (spec 03's capacity check). The text is oasst2 (Apache-2.0, `just fetch-corpus`, SHA-256 `a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751`). Judge scores are written for 20% of completed traces, with new versions and tombstones of recent scores. Deletions register `Traces` every 10 minutes and `User` every hour. Retention runs hourly for every project, with the boundary `keep_from = (now − 4 h).date − retention_days` in UTC (default `retention_days` 0, so only today is kept, and yesterday until 04:00 UTC). The 4 h lag lets a day close (spec 03 §5) before retention drops it. Queries run at 1 per second (`CETRA_SOAK_QPS`) in the mix list 30, filter 15, detail 25, sessions 10, metrics 10, scores 10.
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

No run is recorded yet.

### The VM dry run: rate and calibration

The dry run runs on ubuntu-vm: a VM with 12 vCPUs, 94 GB of RAM and a 500 GB virtual disk whose file lives on another server's NVMe. It is a dry run on a virtual disk, on a VM other work shares, not publishable bare-metal numbers. It runs 48 h, so that the growth rules are enforced with `retention_days` 0 (§ Rules), in local mode.

The disk budget for the run was about 150 GB at peak. With `retention_days` 0, a run keeps up to 28 h of data (yesterday until 04:00 UTC, then today), so the default 2,000 spans/s did not fit:
- **Calibration.** One hour at 2,000 spans/s (`CETRA_SOAK_HOURS=1`, label `cal`), and the first 9 h of an earlier run at 400 spans/s that was stopped when the plan changed. Both stored about 1.15 KB per span on disk (14.6 GB of objects for 12.9 M spans after 9 h; 8.7 GB for 7.2 M spans after 1 h, counting files GC had not collected yet). The generator's payload averages 4.6 KB per span, so the stored data is about 4× smaller.
- **Rate.** 28 h at R spans/s takes about R × 116 MB, plus 15% for compaction outputs and files waiting for GC. The WAL stays under 0.1 GB, and local mode has no read cache. `CETRA_SOAK_RATE=1000` gives about 116 GB of data and 135 GB at peak, under the budget, and leaves room under 2,000.
- **What the calibration found.** `settled` failed on a day close that could never fit the compaction pool (M7-1 in `hardening.md`, fixed before the 48 h run). `rss_max` failed (8.49 GiB, M7-2) and `ingest_rate` failed (0.973 of 2,000 spans/s in the last window, M7-3). The oracle, orphans, retention, deletions and `log_errors` passed. The stopped 400 spans/s run crossed 04:00 UTC and dropped the previous day on time (24 → 12 partitions), with no failure, orphan or retention violation in 9 h.

## Claims

| claim | evidence (run id, metric, value) | caveats |
|---|---|---|

No claim is made yet. A claim names the run's rate, machine and duration, and rests only on rules that read `Pass`.
