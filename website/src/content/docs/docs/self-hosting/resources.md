---
title: "Resources"
description: "How much memory, disk and how many file descriptors cetra needs, and the settings for a small machine."
---

This page tells you what to give cetra, and which settings to lower when the machine is small.

cetra keeps recent spans in memory, writes them to Parquet files, merges the files in the background and answers queries with a shared memory pool. Each part has a budget you can set. All the keys are in the [configuration reference](/docs/self-hosting/configuration-reference/).

## Disk

- Set `storage.min_free_bytes` to cover at least one second of the peak write rate (the WAL plus the outputs of flushes and compaction). cetra measures free disk once a second, so a smaller value can let the disk fill. The default is `"2GiB"`.
- When free disk falls below `storage.min_free_bytes`, cetra refuses new spans until space is free again. It does not fail or corrupt its files.
- Keep the data directory on a local disk (SSD or NVMe), not on NFS: the directory lock is not reliable there. With [object storage](/docs/self-hosting/s3/), the Parquet files and media leave the data directory, but the WAL and the databases stay on it.
- With object storage, the read cache lives under `<data_dir>/cache`. `storage.query.read_cache_bytes` sets its size.

## Memory

The memory cetra needs is the sum of these budgets, plus about 200 MiB of base:

| Budget | Setting | Default |
|---|---|---|
| In-memory buffers for spans and scores, each on its own | `2 × storage.memtable.hard_limit` | `"1GiB"` each |
| The copy a flush makes while it sorts | `2 × storage.flush.max_bytes` | `"256MiB"` each |
| Merges | `storage.compaction.max_memory` | `"1GiB"` |
| Parquet metadata cache | `storage.query.metadata_cache_bytes` | `"512MiB"` |
| The pool shared by queries | `storage.query.max_memory_total` | half the machine's memory, at most `"8GiB"` |
| Requests being ingested | `ingest.max_inflight_bytes` | `"256MiB"` |

Resident memory also holds memory that no budget counts, and that part grows with ingest. Leave headroom beyond the sum. With the default settings, give cetra 8 GiB or more. cetra logs a warning at start when the budgets add up to more than the memory limit it detects, but it does not size them from the machine.

## A small machine

For a machine with 2 to 4 GiB of memory, set:

```toml
[storage.memtable]
hard_limit = "128MiB"

[storage.flush]
max_bytes = "32MiB"
upload_concurrency = 2

[storage.compaction]
max_memory = "128MiB"
threads = 1

[storage.query]
max_memory_total = "256MiB"
max_memory = "128MiB"
initial_memory = "16MiB"
metadata_cache_bytes = "64MiB"
target_partitions = 2
```

Keep `storage.query.target_partitions` low on a small pool: each partition of a sort reserves memory, and a pool that is too small for the partitions refuses even a short trace list. A machine with 2 GiB is the lower limit; use 4 GiB if you can. If the process is killed for lack of memory, lower the budgets further or add memory. Setting `MALLOC_ARENA_MAX=2` can reduce the resident size on glibc.

## File descriptors

cetra holds a small number of descriptors: one for the lock, three for `catalog.db`, one per open WAL segment, and temporary files while flushes and compaction run. A query that spills to disk holds one descriptor per spill file. Sockets for clients come on top.

Set the soft limit to at least 4,096. Below 32 descriptors, cetra cannot run, and between 32 and 256 it reports errors for queries and compaction that it cannot serve. With systemd, `LimitNOFILE=` sets it; Docker and Kubernetes usually give the container a high limit already.
