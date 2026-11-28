# 03 — Storage engine

The columnar store holds three tables: `spans`, `traces` (derived partial summaries) and `scores` (01). It is a small LSM-style engine: **WAL → memtable → L0 Statam files → L1 Statam files** (with optional 1:1 sibling `.stpk` payload packs for L1 `spans`, D-ST-1, D-ST-2), with a catalog in its own SQLite file and `cetra-query` as the query engine over `cetra-column` batches.

Configuration keys named in this spec (`wal.sync`, `flush.max_bytes`, …) live under the `[storage]` section of the server config (12, D90).

`cetra-storage` is policy-free: it never reads `meta.db`. Retention days, project lifecycle and deletion scopes reach it through its API (D83). The design behind this spec, with its public API and milestones, is `docs/plans/a1-storage/00-design.md`; the research behind it is `docs/research/storage/`.

## Layout on disk / object store

```
<data_dir>/
  LOCK                    exclusive flock: one process per data directory (12)
  meta.db                 SQLite, server metadata (04)
  catalog.db              SQLite, storage catalog (§4), synchronous=FULL
  wal/
    spans/00000000000000000042.wal
    scores/00000000000000000007.wal
  tmp/
    query/                query spill files (§6), emptied at open
    compaction/           S3-mode compaction outputs being written (§5)
  cache/                  S3 read cache (§6), only with an S3 object store
    <file name>/size, <file name>/<block index>
  objects/                only when object store = local
    tables/<table>/project=<project_id>/day=<YYYY-MM-DD>/<level>-<uuidv7>.statam
    tables/spans/project=<project_id>/day=<YYYY-MM-DD>/<level>-<pack_uuidv7>.stpk  (L1 spans, 1:1 optional)
    media/<project_id>/<sha256>
```

With `storage.object_store = s3://bucket/prefix`, the `objects/` tree lives in the bucket; `catalog.db`, `meta.db` and `wal/` stay on local disk, which **must be persistent** (an ephemeral disk loses up to `flush.max_age` of acknowledged data on machine loss — documented). Absent, `object_store` is `file://<data_dir>/objects`. A local path is `file://<absolute path>`.

A local object write lands in the destination directory as `.<filename>.tmp-<uuidv7>`, then `sync_all`, rename onto `<filename>`, and an fsync of that directory. A write, sync or rename that fails removes the temp file. Startup deletes leftover files whose names start with `.` and contain `.tmp-`. A local delete unlinks the file and fsyncs its directory before the catalog row is removed. Startup fsyncs the parent of every directory of the data directory layout and of the local object tree, the root's own entry included, and fsyncs each object directory it walks for leftover temp files, so an entry that a crashed process created is durable before it is used.

**Partition key:** `(project_id, day)`.
- For `spans` and `traces`, `day` is the trace's **anchor day** (§ Trace anchor). It equals the UTC date of the row's own `start_time` or differs from it by at most one day.
- For `scores`, `day` is the UTC date of `created_at`.

A late span lands in its trace's anchor partition, creating a new L0 file there; that is fine.

Spans are expected to carry plausible start times. Normalization rejects spans that start before 2000-01-01, before `ingest_time − retention_days`, or more than one day after `ingest_time` (02 rule 15, D85).

## Trace anchor

A trace is anchored to the partition day of its **first partial**: the first time a flush writes rows for it, they go to the UTC day of their `start_time`. Later rows of the same trace go to that anchor day if their own UTC day is within one day of it; otherwise they go to their own day. Retention and compaction therefore handle a trace that crosses midnight as one unit (D86).

Only traces near midnight need remembering. The storage keeps a map `(project_id, trace_id) → anchor day` for every trace with a row starting within `anchor.window` (default = `trace_lookback`, 1 h) of a UTC midnight, on either side. The nearest midnight wins; exactly noon rounds up.

- **Assignment is per row.** A row goes to its trace's anchor day when its own UTC day is at most one day away, and to its own day otherwise. A row two or more days from the anchor stays in its own day, where the one-day query slack finds it.
- **Expiry runs on ingest time.** An entry made by a flush expires at that flush's largest `ingest_time` + `2 × anchor.window`; any flush with rows for a trace that already has an entry refreshes that entry's deadline the same way, even when none of its rows is near midnight. Expired entries are dropped after each commit once the clock (the largest `ingest_time` any flush has committed) passes their deadline. Because this counts ingest time, not event age, backfilled data (old events ingested now) is anchored like live data.
- **Rebuild at startup.** The map is rebuilt from the active `traces` files whose `max_ingest_time` is within `2 × anchor.window` of the clock. Only the row groups whose `last_ingest_time` statistics have a max in that window are read, projected to `trace_id`, `start_time`, `end_time` and `last_ingest_time`; rows with `last_ingest_time ≥ clock − 2 × anchor.window` and `start_time` or `end_time` near midnight restore an entry in the file's partition day. A partial's `start_time` is the trace's earliest start, so `end_time` is what restores a trace whose child ends near midnight. If several files name a trace, the earlier day and the later expiry win.
- **Replayed rows.** Rows replayed from the WAL get their anchor when they flush.
- **Known limit.** A trace with no row near midnight before a flush, and rows past midnight later, gets an entry only from its later rows. Example: rows at 22:00 flushed, then rows at 00:30. Its earlier rows stay in the earlier day. The anchor guarantees one day only for traces whose rows reach the window before midnight.

Because rows may sit in the day before or after their own date, day pruning (§6) widens the day range by one day on each side. File pruning by `min_time`/`max_time`, which hold the real times, stays exact. Memtable pruning stays exact.

## 1. Write-ahead log

One WAL per table (`spans`, `scores`), each a sequence of segment files. A segment is named with the seq of its first record, zero-padded to 20 digits, plus `.wal`. Segments rotate at `storage.wal.segment_bytes` (default 64 MiB). Rotation happens between groups, so one group can make a segment larger than the limit.

Each segment starts with an 8-byte header (§ 12): the magic `LWAL`, the format version as a `u16`, and a `u16` of flags that must be 0. The header is written and fsynced when the segment is created, before any record, so no crash leaves a record behind a torn header. Records follow it; offsets in errors count from the start of the file.

Record format. Every integer is little-endian. The outer WAL checksum covers `seq` as 8 little-endian bytes, then `kind` as 1 byte, then the payload. The payload is one `cetra-column` binary batch frame (`CBM1`, `FRAME_VERSION = 1`), carrying a 20-byte frame header (`magic(4) + version(1) + flags(1) + reserved(2) + uncompressed_len(4) + payload_len(4) + crc32c(4)`) and optional LZ4-frame body compression (`FLAG_LZ4 = 0x01`). Both the outer WAL record and inner `CBM1` frame reject payloads longer than `256 MiB` (`MAX_PAYLOAD`) and decoded sizes exceeding `1 GiB` (`MAX_DECODED_BYTES`) symmetrically on encode and decode. Before decoding, the frame reader checks magic, version, flags (`0` or `FLAG_LZ4`), zero reserved bytes, frame CRC-32C, column count (`≤ 1024`), schema/length validity (including `RLE` `run_count ≤ len` and cumulative logical element caps), and the LZ4 expansion-ratio bomb limit (`uncompressed_len ≤ 256 × payload_len + 64`). Decoding never panics on corrupt or truncated frames; any violation is corrupt.

```
u32  payload_len
u32  crc32c(seq || kind || payload)
u64  seq            monotonically increasing per table
u8   kind           1 = row batch; any other value, including the reserved 2, is invalid
[..] payload        cetra-column CBM1 binary batch frame (one Batch, table schema, already normalized),
                    with optional LZ4-frame compression
```

**Group commit.** Ingest tasks send `(Batch, oneshot ack)` to a single WAL writer task per table over a bounded channel. The writer:
1. drains everything available (up to 8 MiB or 2 ms after the first item);
2. writes all records with one `write_all` and calls `fdatasync`;
3. inserts the batches into the active memtable in `seq` order and publishes a new read state (§4);
4. completes every ack.

This gives one fsync per group, so throughput is bounded by disk bandwidth, not fsync latency. Because one task both writes the WAL and fills the memtable, memtable order equals WAL order, and an acknowledged write is visible to the next query. Config `wal.sync = "always" | "<duration>"` (default `"always"`). A duration such as `"5ms"` acknowledges after the write and fsyncs at most that often; it trades durability for speed and is documented: a power loss can lose the last interval, a process crash loses nothing.

The `wal/<table>/` directory is fsynced when a segment is created or deleted.

*Provisional, pending the coordinator's decision on the WAL rotation rule (R5.8; not yet in force):* A segment that cannot be created is not a write error: the writer keeps the current segment and retries the rotation after the next group. Until that is recorded, a failed rotation is final, as below.

A segment is fsynced before a newer one is created, at rotation and at open, so only the last segment can have a torn tail. In interval mode, a power loss therefore loses a suffix of each WAL: the records acknowledged since that WAL's last fsync.

**Write errors are final.** If a write or `fdatasync` fails, the writer fails every pending ack (`WalFailed`), refuses further writes and reports the storage unhealthy; the operator restarts the process. The failed fsync is never retried, because the kernel may already have dropped the dirty pages. The write-and-sync step runs on a blocking thread and cannot be cancelled halfway. A group whose `fdatasync` failed is left as written: a restart may replay it, although its appends answered `WalFailed`. Clients retry on 503, and replayed duplicates merge like any re-send (§2, §5).

**Checkpoint.** The catalog stores `flushed_seq` per table (highest WAL seq whose rows are in Statam files). WAL segments are deleted only after the catalog transaction that advanced `flushed_seq` has returned (§2). Segment `k` is deleted when a later segment exists and `first_seq(k + 1) - 1 <= flushed_seq`. The newest segment stays, because the writer may still be appending to it (D120). After every flush that commits, the flush first asks the writer to start a new segment when the open one holds records (D-A1-14): the writer syncs the open segment when it holds unsynced bytes, creates the next one at `next_seq` (which fsyncs `wal/<table>/`), and acknowledges. The same flush then deletes every segment whose records are all flushed. A segment that also holds records newer than the flush stays until the next flush deletes it, so flushed rows leave the WAL by the end of the next flush cycle. Replay skips the flushed records in such a segment. The extra bytes are at most one segment (`wal.segment_bytes`).

**Recovery.** At startup, before ingestion opens, for each table:
- read segments in order, skip records with seq ≤ `flushed_seq`, and insert the rest into the memtable;
- a record with a bad CRC or short read at the **tail** of the last segment is a torn write: truncate the segment there;
- a bad record anywhere else is corruption: refuse to start unless `--wal-skip-corrupt`;
- a segment whose header is not this build's (an unknown magic, a newer version or flags) refuses the open, is never truncated, and is not skipped by `--wal-skip-corrupt`; a segment shorter than the header that is a prefix of it holds no records (a torn creation): it is truncated to empty when it is the last one and skipped when it is not;
- the next seq is `max(last valid record's seq, flushed_seq) + 1`, so a sequence number is never reused, even when `every_ms` lost a tail.

## 2. Memtable

Per table: `active` (accepting inserts) and at most one `frozen` (being flushed). Each is a map `partition → Vec<MemBatch>` (holding `Arc<cetra_column::Batch>`, time bounds, and lazily cached trace partials) plus byte accounting. Inserts append the WAL'd `Batch` (split by partition) — no row-by-row building, so inserts are cheap.

- **Accounting** uses `Batch::memory_bytes()` (the real memory footprint of the `cetra-column` buffers), not the encoded size: small OTLP batches carry a fixed overhead per column.
- **Coalescing:** when a partition holds more than 64 batches smaller than 1 MiB, they are concatenated into one batch.

**Flush triggers:** active size ≥ `flush.max_bytes` (default 256 MiB), oldest row age ≥ `flush.max_age` (default 120 s), or shutdown.

**Backpressure:** if a table's `active + frozen` exceeds `memtable.hard_limit` (default 1 GiB; each table on its own, so both together can hold twice that) ingestion answers `503 Retry-After: 1` / `UNAVAILABLE` (D-G-3; `429` is kept for per-org limits).

### Flush

1. **Freeze.** Under the table-state write lock: move `active` → `frozen`, create a new empty `active`, record `frozen_max_seq`.
2. **Prepare, outside the lock.** For `spans`, first regroup each project's frozen rows by trace and assign every trace's rows to its anchor day (§ Trace anchor): the memtable partitions by each row's own day, files by the anchor day. Then, per output partition:
   - drop the partitions of dropped projects (§7) and those older than the project's retention watermark (§8), then the rows covered by pending deletions (§7). The tombstones and project states are read once per attempt; every output records the highest `deletion_seq` known then. A partition left with no row writes no file, and a flush left with no file commits only `flushed_seq`;
   - drop duplicate `(trace_id, span_id)` rows, keeping the greatest `ingest_time` (spans);
   - sort (spans: `trace_id, start_time`; scores: `created_at`);
   - for `spans`, also compute the **trace partials** of exactly the rows of that spans file, one per trace, with the merge operator (spec 01 merge rules, D152), for a `traces` file derived from it (§3).
3. **Stage.** One small catalog transaction records every output path in `staged_objects`.
4. **Write.** Write the files, up to `flush.upload_concurrency` (default 8) at a time. Each file is durable before the next step:
   - local disk: write to a temp file in the target directory, `sync_all`, rename, fsync the directory;
   - S3: the PUT or multipart upload has completed.

   An upload that fails ambiguously (error, but the object may exist) is retried under a new name; the old name stays staged and GC removes it.

   A multipart upload that fails after it started (a part, the part source or `complete`) is aborted, best effort. An abort cannot remove an object that `complete` did create, so this is safe after an ambiguous `complete`. Uploads that no abort reached remain on the bucket: those whose abort failed, those whose `CreateMultipartUpload` answer was lost, and those cut by a crash or a cancelled task. The bucket's lifecycle rule removes them (`https://cetra.sh/docs/self-hosting/s3/`).
5. **Commit.** One catalog transaction, through the committer (§4):
   - insert the new `data_files` rows as `active`, and the `file_sources` rows;
   - remove the `staged_objects` rows;
   - set `flushed_seq = frozen_max_seq`, compare-and-set (it must be greater than the current value).

   The same commit publishes a read state with the new files and without `frozen`.
6. **Truncate.** Delete the WAL segments covered by `flushed_seq`, keeping the newest (§1, D120).

Step 5 makes the switch atomic for readers: a query snapshot sees either (frozen, without the new files) or (the new files, without frozen), never both or neither.

A failure in steps 2–5 keeps `frozen` and retries that same view under new file names. The wait starts at 1 s, doubles, and is capped at 60 s, and it resets after an attempt that writes nothing or commits. A shutdown signal ends the wait that is in progress. The two flush tasks then have 30 s together to finish; a task still running is cancelled, and its rows stay in the WAL for the next open. The staged names of a failed attempt stay for GC (§5). A failure of step 6 does not undo the commit: the sequence is remembered and truncation is retried with the same backoff. The task also wakes on a timer (50 ms when no age deadline is sooner), so age and data recovered at startup flush without another append.

## 3. Statam files and `.stpk` payload packs

Data files use Cetra's native columnar format `cetra-statam` (`.statam`, magic `STM0` / postscript `0MTS`, wire format version 1, CRC-32C on every block, frame and trailer; D-ST-1). L1 `spans` files additionally own at most one sibling content-addressed payload pack (`.stpk`, magic `SPK0` / postscript `0KPS`; D-ST-2).

| Setting | Value |
|---|---|
| Write profiles | **`L0Fast`** (flush, level 0): self-contained `.statam` (no `.stpk`; large payloads stored inline or in file-local L0 payload frames), raw-block LZ4 (`lz4_flex::block`) with incompressibility guard. **`L1Compact`** (compaction, level 1): Zstd level 3 on narrow blocks and `.stpk` frames, flat-map shredding, and (for `spans`) an optional 1:1 sibling `.stpk` chunk pack. |
| Row group (PAX) | target 131,072 rows (`ROW_GROUP_SIZE`) or 64 MiB encoded bytes (hard cap 262,144 rows), grouped into contiguous `Core` + `WideRow` locality groups (plus `ExternalPayload` reference columns) |
| Mini-block | target 8,192 rows (`MINI_BLOCK_SIZE`), framed (`SB`, 16 B header + CRC-32C) with per-mini-block min/max zone maps |
| Hot Tail (`footer_size`) | schema metadata (`FileSchemaMeta`), row-group/column/block directories, shredded-map directory, inline hot bloom filters, lazy-index directory, and pack IDs; LZ4-compressed, capped at 1 MiB. `data_files.footer_size` stores `hot_tail_bytes`; remote cold opens fetch `max(footer_size, DEFAULT_TAIL_HINT)` (`DEFAULT_TAIL_HINT = 192 KiB`, clamped to 1 MiB) in 1 ranged GET |
| Dictionaries | file-level dictionaries (`DT01`) for low-cardinality string columns (`status`, `span_kind`, `model`, `name`, `service_name`, `environment`, `release`, `provider`, …); off for high-cardinality IDs and heavy payloads. Native `Dict`/`Rle`/`Const` encodings pass directly into `cetra-column` / `cetra-query` |
| Shredded maps (`L1Compact`) | `metadata`, `attributes`, `resource_attributes` shred frequent keys (up to 64 keys per map) into typed sub-columns with `in_map` + `value_null` bitmaps, leaving cold keys in a residual block |
| Bloom filters | 8-lane SIMD Split-Block Bloom Filters (`BF01`/`BF02`, `statam_bloom_hash`) stored in the Hot Tail or Lazy Index sections for `trace_id`, `session_id`, `user_id` on `spans` and `traces`, and `trace_id`, `score_id` on `scores` |
| `.stpk` payload packs (L1 `spans`) | 1:1 sibling pack per L1 `spans` file (omitted when 0 external chunks are emitted, D-ST-2): structural JSON / FastCDC chunking, BLAKE3-128 dedup (`ChunkHashTable`), 512 KiB target frames (dedicated frames for large chunks), `TS02` per-frame trigram skip blooms + `EdgeTable` for exact substring search, optional trained Zstd `DictSection` (discarded on GDPR rewrite), `PackToc` + 112 B `PackPostscript` (`data_files.pack_toc_size` records TOC + Postscript bytes for 1-GET open) |
| Sort order (within a file) | `spans`: `trace_id, start_time` · `traces`: `start_time, trace_id` (the `trace_id` tie-break makes files byte-identical across runs) · `scores`: `created_at` (recorded in `FileSchemaMeta::sort_cols`) |
| Time slicing (across files) | every file covers a contiguous `[min_time, max_time]` slice; see below |
| File metadata | `FileSchemaMeta` (`table_kind`, `sort_cols`, `shredded_maps`, `pack_ids`) in the Hot Tail, and wire `FORMAT_VERSION` in `FileHeader`/`Postscript` (and `PackHeader`/`PackPostscript`) (§12) |

**Time-sliced files, trace-sorted rows** (same pattern as Opik, LangWatch and Laminar, which partition by time and sort by trace ID inside the partition):

- *Across files*, data is organized by time: L0 files cover one flush window; compaction merges inputs in time order and cuts outputs at time boundaries, so each output file covers a contiguous, non-overlapping time slice. Time-range scans (metrics, alerts, span search) prune at file granularity using `min_time`/`max_time`.
- *Within a file*, spans are sorted by `trace_id, start_time`: opening a trace reads one contiguous run of rows (usually one mini-block) per file instead of rows scattered over many row groups.
- The time-ordered access path for lists is the `traces` table (sorted by `start_time`), the equivalent of Laminar's time projection / LangWatch's `trace_analytics`.

**`traces` files are derived from `spans` files** (D88). Every `traces` file is computed from a set of `spans` files of the same partition, recorded in `file_sources`. The sets of the active `traces` files are disjoint and together cover every active `spans` file of the partition. Whatever replaces `spans` files (compaction, deletion rewrite, retention) replaces the `traces` files derived from them in the same commit, recomputing their partials from the new `spans` files. Duplicate spans therefore stop counting twice once their spans files are merged, and `traces` needs no deletion logic of its own.

**Schema evolution:** only adding nullable columns. Each file carries its own `FileSchemaMeta`; `StatamSource` fills missing nullable columns with `Const(Null)` columns matching the target `cetra-column` schema. Renames or type changes need a new column. All Statam read/write usage in `cetra-storage` is isolated inside `statam_io/` and `engine/statam_source.rs`. `arrow` is not a dependency of any shipped crate; `parquet` (`default-features = false`, `zstd` only) remains strictly in `engine/parquet_export.rs` for user-facing SQL `.parquet` exports (D-QE-8, D-QE-12, D-ST-1).

## 4. Catalog

`catalog.db` belongs to `cetra-storage` (D83): its own migrations (`sqlx::migrate!`, forward-only), its own writer connection, `journal_mode=WAL`, `synchronous=FULL` (a commit that advances `flushed_seq` must survive a power loss, since the WAL segments it covers are deleted afterwards). `PRAGMA user_version` holds the data directory's format version (§ 12); `open` refuses a newer one before it migrates.

```sql
CREATE TABLE data_files (
  id TEXT PRIMARY KEY,                -- UUIDv7
  table_name TEXT NOT NULL,           -- 'spans' | 'traces' | 'scores'
  project_id TEXT NOT NULL,
  day TEXT NOT NULL,                  -- partition day 'YYYY-MM-DD'
  level INTEGER NOT NULL,             -- 0 | 1
  path TEXT NOT NULL,                 -- relative to object store root
  size_bytes INTEGER NOT NULL,
  footer_size INTEGER NOT NULL,       -- Hot Tail byte length; open with one ranged read, no HEAD
  row_count INTEGER NOT NULL,
  min_time INTEGER NOT NULL, max_time INTEGER NOT NULL,
  min_ingest_time INTEGER NOT NULL, max_ingest_time INTEGER NOT NULL,
  fully_merged INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT 'active',    -- 'active' | 'obsolete' | 'quarantined'
  created_at INTEGER NOT NULL, obsoleted_at INTEGER,
  deletions_applied_seq INTEGER NOT NULL DEFAULT 0, -- §7 (added by migration 0003)
  pack_id BLOB,                       -- 16-byte UUIDv7 of 1:1 sibling .stpk pack (migration 0008, D-ST-2)
  pack_path TEXT,                     -- relative path to .stpk; NULL unless L1 spans with chunks
  pack_size_bytes INTEGER,            -- .stpk byte size
  pack_toc_size INTEGER               -- .stpk TOC + Postscript byte length for 1-GET open
);
CREATE INDEX data_files_lookup ON data_files(table_name, project_id, day) WHERE state = 'active';
CREATE INDEX data_files_gc ON data_files(obsoleted_at) WHERE state = 'obsolete';
CREATE INDEX data_files_pack_path ON data_files(pack_path) WHERE pack_path IS NOT NULL;

-- traces file → the spans files it was computed from (§3)
CREATE TABLE file_sources (
  traces_file_id TEXT NOT NULL REFERENCES data_files(id) ON DELETE CASCADE,
  spans_file_id TEXT NOT NULL REFERENCES data_files(id) ON DELETE CASCADE,
  PRIMARY KEY (traces_file_id, spans_file_id)
);
CREATE INDEX file_sources_by_spans ON file_sources (spans_file_id);

-- objects being written; anything left here after a crash is garbage (§5 GC)
CREATE TABLE staged_objects (path TEXT PRIMARY KEY, created_at INTEGER NOT NULL);

CREATE TABLE table_state (table_name TEXT PRIMARY KEY, flushed_seq INTEGER NOT NULL);

CREATE TABLE projects (                -- only dropped projects (§7)
  project_id TEXT PRIMARY KEY,
  state TEXT NOT NULL,                -- 'dropping' (media not yet deleted) | 'dropped'
  dropped_at INTEGER NOT NULL
);
CREATE TABLE retention_watermarks (project_id TEXT PRIMARY KEY, keep_from TEXT NOT NULL);  -- §8

CREATE TABLE media (                   -- §9
  project_id TEXT NOT NULL, sha256 TEXT NOT NULL,   -- 64 lowercase hex
  mime_type TEXT NOT NULL, size_bytes INTEGER NOT NULL,
  first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
  newest_span_start INTEGER NOT NULL DEFAULT 0,   -- ns, newest span that referenced it (D-P-21)
  PRIMARY KEY (project_id, sha256)
);

CREATE TABLE deletions (               -- §7; the request itself is in meta.db (04)
  id TEXT PRIMARY KEY,                -- same id as meta.db deletions.id
  deletion_seq INTEGER NOT NULL UNIQUE,
  project_id TEXT NOT NULL,
  kind TEXT NOT NULL,                 -- 'traces' | 'user'
  cutoff INTEGER NOT NULL,            -- the request's created_at
  status TEXT NOT NULL DEFAULT 'pending',  -- 'pending' | 'applied'
  registered_at INTEGER NOT NULL, applied_at INTEGER
);
CREATE INDEX deletions_pending ON deletions (deletion_seq) WHERE status = 'pending';
CREATE TABLE deletion_traces (deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
                              trace_id BLOB NOT NULL, PRIMARY KEY (deletion_id, trace_id));
CREATE TABLE deletion_users (deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
                             user_id TEXT NOT NULL, PRIMARY KEY (deletion_id, user_id));
CREATE TABLE deletion_sessions (deletion_id TEXT NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
                                session_id TEXT NOT NULL, PRIMARY KEY (deletion_id, session_id));
```

At startup all active rows are loaded into an in-memory structure (rejecting partial-NULL `pack_*` columns, `pack_id` length ≠ 16, or non-positive `pack_size_bytes` / `pack_toc_size`):

```
Catalog { tables: { table → { project_id → BTreeMap<day, Vec<Arc<FileMeta>>> } } }
FileMeta { id, path, level, size, footer_size, rows, min_time, max_time,
           min_ingest_time, max_ingest_time, fully_merged, deletions_applied_seq,
           pack: Option<PackMeta { id, path, size, toc_size }> }
```

`FileMeta::total_bytes()` returns `size + pack.size` (or `size` when `pack` is `None`), used for compaction sizing, disk reservations, retention byte reporting, and scan work distribution.

**Read state.** Readers load one `ReadState` (an `Arc` behind a `std::sync::RwLock`, held only to clone the `Arc`): the in-memory catalog, the memtable batches (`active` and `frozen` of both tables, as `MemBatch` lists holding `Arc<cetra_column::Batch>`), the tombstone sets (§7) and the project states (retention watermarks §8, dropped projects §7), all from the same publish. A row is therefore seen exactly once, in a memtable or in a file.

**Committer.** Every change to the set of files is a catalog edit applied by one committer, which runs the SQLite transaction and publishes the new `ReadState` under one mutex, so commits and snapshots are totally ordered. This covers flush, compaction, deletion rewrite, retention, project drop and GC. An edit has:
- the files to add, whose `path` (and `pack.path`, when `Some`) must be in `staged_objects`;
- optionally `unstage`: unused staged paths (such as a pre-staged `.stpk` path whose compaction emitted 0 external chunks) removed from `staged_objects` in the same transaction;
- the files to remove, with their expected state (normally `active`);
- optionally a new `flushed_seq` (compare-and-set: it must be greater);
- optionally files whose `deletions_applied_seq` rises without a rewrite (§7); each must be active and the new value greater;
- optionally active files to mark `quarantined` (§10), under the same dependants rule as removed files.

`collect_staged_path` checks both `data_files.path` and `data_files.pack_path` (using `data_files_pack_path`). `needed_keys()` and `known_keys()` include `pack_path` for every row where `pack_path IS NOT NULL`. Online backups (`snapshot.rs`) query active `path`, `size_bytes`, `pack_path`, and `pack_size_bytes`, emit both `.statam` and `.stpk` entries as `LiveObject`s, and sort all data-file `LiveObject`s by `key` before appending media.

An added file is refused when its project is dropped (§7) or its day is before the project's retention watermark (§8), so a flush or compaction prepared before a drop or a retention cannot bring the data back. Retention and project drop commit through the same path, with their own rows (`retention_watermarks`, `projects`, `deletions`) written in the same transaction.

If a precondition fails, the transaction rolls back and the job gets a conflict: its outputs stay staged and are collected by GC, and the job retries later.

A commit that fails with a SQLite I/O error (`SQLITE_IOERR`, `_READ`, `_WRITE`, `_FSYNC`, `_DIR_FSYNC` or `_TRUNCATE`) stops the committer: every later catalog call fails until a restart, and health reports it (D-R-6). The commit's frames may already be in `catalog.db-wal`, so the restart, not a retry, decides whether it happened.

**Claims.** Background jobs claim their input files in memory before reading them, so compaction and the deletion rewriter never work on the same file. Claims don't survive a restart and need not: an interrupted job is simply redone.

**Metadata cache.** One process-wide `MetadataCache` (a bounded LRU cache in `QeRuntime`, keyed by `MetaCacheKey::{File(FileId), Pack([u8; 16])}`) caches decoded `Arc<StatamFileMeta>` (weighted by `StatamFileMeta::memory_bytes()`) and `Arc<PackToc>` (weighted by `PackToc::memory_bytes()`), shared across queries, compaction, and maintenance. `query.metadata_cache_bytes` (default 512 MiB; zero disables it) caps it. Hot bloom filters embedded in `StatamFileMeta` stay cached with the file metadata; lazy bloom and trigram sections are fetched on demand from the object. Files are immutable, so entries never go stale; GDPR rewrites (§7, D-ST-3) and quarantine/rebuild (§10) explicitly invalidate retired file and pack entries.

## 5. Compaction

A background task runs a **compaction pass** every `compaction.interval` (default 30 s). A pass looks at every `(table, project, day)` partition of `spans` and `scores` in the current catalog, picks at most one job per partition, and runs the jobs with at most `compaction.threads` (default `max(1, cores / 4)`) at a time. `traces` files are never compacted on their own: they follow their `spans` files (below).

Times below are wall-clock nanoseconds (`now`). A file's age is `now` minus its `max_ingest_time`.

**Policy, per partition.** A partition with any file claimed by a running job gets no job. All size thresholds, size-ratio checks, slice counts and byte reports use `file.total_bytes()` (`.statam` + sibling `.stpk`).
- *Day close* is due when `now ≥ end of the partition day + compaction.close_delay` (default 2 h), the partition is not closed, and its newest L0 file, if any, is at least `compaction.close_delay` old. A partition is **closed** when:
  - `spans`: it has no L0 file, and either it has at most one file or its files total more than `compaction.target_file_size` (default 512 MiB);
  - `scores`: every file has `fully_merged = true`.

  The day close job takes every file of the partition when their total is at most `compaction.target_file_size`, and for `scores` always. Otherwise (`spans` only) it takes only the L0 files. The outputs of a `spans` job that took every file of the partition, and of a `scores` day close, have `fully_merged = true`. A `scores` L0 merge never does, since it keeps deleted versions. A new L0 file in a closed day (late data) makes it not closed, so another day close runs after `compaction.close_delay`.
- *L0 merge*, when no day close is due: when the partition has at least `compaction.l0_files` (default 8) L0 files, or at least `compaction.l0_bytes` (default 512 MiB) of L0, or at least two L0 files and the newest is at least `compaction.l0_age` (default 10 min) old. Inputs are all L0 files of the partition plus L1 files chosen by the **size ratio**: take the L1 files in increasing size order, and add each one while the output so far (the summed size of the inputs chosen) is at least `compaction.size_ratio` (default 4) times its size; stop at the first that fails. A partition with one big L1 file and a trickle of L0 files therefore gets a second L1 file instead of rewriting the big one every few minutes. A job needs at least two input files.

**Slicing.** A job's output is cut into `n = max(1, ceil(input bytes / compaction.target_file_size))` L1 files, each holding one contiguous range of the table's time column (`start_time` for `spans`, `created_at` for `scores`). The boundaries are the time values at ranks `⌊len × i / n⌋` (i = 1 … n − 1) of the sorted time values of all input rows; equal boundaries collapse into one. A slice holds the rows with `lower ≤ time < upper`; the first slice has no lower bound and the last no upper bound. Each slice is a streaming k-way merge of the inputs' sorted runs (`spans`: `trace_id, start_time`; `scores`: `created_at`), filtered to the slice. A trace crossing a slice boundary is split across two files, which is fine.

**During a merge:**
- `spans`: duplicate `(trace_id, span_id)` rows within a slice are collapsed, keeping the greatest `ingest_time` (the later row on a tie), as at flush. Two copies of a span whose `start_time`s fall in different slices both stay.
- `traces`: each `spans` output gets its own `traces` output, computed from exactly its rows by the merge operator (one partial per trace, D152), at the same level and with the same `fully_merged`. The `traces` files derived from the `spans` inputs (§3) are removed in the same commit. A day that fully merges therefore ends with one `traces` file and `partial_count = 1` for every trace; a bigger day has one partial per trace and `spans` file the trace reaches (D-A1-1).
- `scores`: last-writer-wins on `score_id`, keeping the greatest `(updated_at, ingest_time)` (the later row on a tie), as the `scores` view does. Rows with `deleted = true` are dropped **only by a day close**, which always takes every file of the partition: every version of a score shares its `created_at`, so they are all among the inputs. Known limit: once a day close has dropped a score's tombstone, a version of that score with an older `updated_at` that arrives later reads as live again. The tombstone is the score's own, from a score delete. GDPR erasure goes through the deletion rewriter (§7) and is not affected. A resurrection needs a writer to send such a version after `compaction.close_delay` (default 2 h) past the end of the score's day. The server stamps `updated_at` at ingest (`crates/server/src/scores.rs`, `times`), so an API retry gets a later stamp, not an older one. The SDKs never send `updated_at`, and their retries end within about a minute, far inside `close_delay`. So a resurrection needs a row written outside that path, or a server clock that steps back. No ingest age bound applies to score times: D85 covers spans only.
- Tombstones (§7): the deletions pending when the job starts are applied to `spans` and `scores` rows before the dedup, and every output (`traces` included) records the highest `deletion_seq` known when the job started. A slice left with no row produces no output.

**Resources:**
- A merge streams its inputs (one batch per input at a time via `StatamSource`) instead of sorting a whole slice in memory. Merge memory across running jobs is capped by `compaction.max_memory` (default 1 GiB), a standalone `GlobalPool` of its own. A batch holds at most 8,192 rows, fewer for large rows: a job sizes its batches so that four batches of every input fit in its share of the pool (`compaction.max_memory / compaction.threads`), estimating a row at 8 times its stored bytes per row (using `total_bytes()`), and never below 64 rows.
- Each `spans` and `scores` output is written while it is merged, into a temp file, then published durably. For L1 `spans`, the job also allocates a `pack_id` (UUIDv7), stages `[path, pack_path, traces_path]`, and opens a streaming `ChunkPackBuilder::new_streaming` temp file owned by `BatchFileWriter` (`StatamWriter::new_with_owned_pack`), streaming compressed `.stpk` frames directly to disk in O(1 frame) RAM. On finish, if `chunk_count > 0`, the `.stpk` pack is finalized and published **before** the `.statam` file (so no committed `.statam` can reference an unpublished pack); if `chunk_count == 0`, the temp pack file is discarded, `pack` is set to `None`, and `pack_path` is added to `CatalogEdit.unstage` so `staged_objects` stays clean (D-ST-2). On local disk each temp file sits in the target directory and is synced and renamed like any object write. With S3 each temp file is `<data_dir>/tmp/compaction/<uuidv7>.tmp`, uploaded, then removed. A failed upload is aborted as at flush (§2). `traces` outputs are small and are built in memory (D-A1-2).
- Before it reads anything, every job (merges and rewrites, local and S3 mode alike, since outputs stream through a local temp file in both) reserves the sum of its `spans`/`scores` input `total_bytes()` against free disk minus the other running jobs' reservations. A job that doesn't fit fails as deferred and a later pass retries it (§10, D-A1-9).

**Commit:** one catalog edit (§4) adds the outputs (`spans` with their optional `pack` and their `traces`, or `scores`), removes any unused `unstage` paths, adds the `file_sources` rows of the new pairs, and marks the inputs and the `traces` files derived from them `obsolete` with `obsoleted_at` = the committer's wall clock at commit. Shutdown stops compaction between slices or before a commit, never inside one. The committer refuses an edit that removes a `spans` file without its active `traces` file, or a `traces` file without every one of its sources.

**GC of obsolete files.** Every 10 min, GC evicts both `pack_path` (if `Some`) and `path` from `ReadCache`, deletes `pack_path` first (ignoring `NotFound`) and then `path` (summing deleted bytes), and removes the `data_files` row only after both deletes succeed (its `file_sources` rows go with it), once `obsoleted_at + gc.grace ≤ now` (`gc.grace`, default 5 min, at least the longest query timeout; at most 30 days. In S3 mode it is also how long a metadata-only backup stays restorable against the live bucket (D-N-20)) **and** no `ReadState` of this process still references the file (reference counts on `FileMeta`). A raise replaces a file's `FileMeta`; the count covers the versions it replaced too. After a restart the counts are gone and the grace alone decides.

**Crash safety:** an output written but never committed is still in `staged_objects`. GC deletes staged objects older than 1 h and their rows, so the bucket is never listed. Startup removes leftover temp files: in the object directories in local mode, and in `<data_dir>/tmp/compaction/` in S3 mode. A job interrupted by a crash or a conflict is redone by a later pass.

A server in restore mode opens the storage with object deletion off (`StorageConfig::delete_objects`, N M3, D-N-34): every object deletion fails and is retried by a later server, so GC, media collection and project drops remove nothing. `gc-orphans` still deletes.

## 6. Query path

For each request, `Storage::query` admits a `ScopedPool` from `QeRuntime`'s `AsyncAdmissionPool` (§ Resource limits, D-A1-8) and returns a `ProjectQuery`: the project's `ScanSource`s (`spans`, `traces`, `scores`) bound to one `ReadState` (catalog, memtable and tombstones, §4), together with the shared `QeRuntime` (`MetadataCache`, `WorkerPool`, `SpillManager`, `target_partitions`), `around` (D154), and `visible_from` (the org's data-window bound, which 05's base views and reads apply to `start_time` on `spans`/`traces` and `created_at` on `scores`, D-P-21, D-P-47). `QueryContext` wraps `ProjectQuery` with an optional SQL `TimeWindow` and registers server-owned SQLite views as `BatchSource` tables (05, D91). Product reads (`cetra_storage::reads`) build direct `cetra-query` push pipelines over `ScanSource` without SQL planning; the SQL console and MCP run through `cetra-query::sql` (05). The sources expose the stored `cetra-column` schemas of 01 (binary IDs, `project_id`), while views expose hex IDs, hide `project_id`, add `sessions`, and apply default exclusions (D89). Because each source is constructed for a single project, the user cannot name another project's data. A dropped project (§7) gets empty sources.

### `StatamSource` & `MemtableSource` (`spans` and `scores` scans)

1. Extract a time range from the `ScanSpec` filters on `start_time` (or `created_at` for `scores`):
   - the comparisons `=, <, <=, >, >=, BETWEEN`;
   - an `OR` gives the union of its sides' ranges; a side that is `start_time IS NULL` on the non-null column is dropped.

   Prune file partitions by `day`, widened by one day on each side for `spans` and `traces` (§ Trace anchor), and files by `min_time`/`max_time`. A `trace_id` lookup (equality or `IN` with 16-byte literals) without a time range, with `QueryOptions::around` set, reads only the `spans` and `traces` files that overlap `around ± 1 day` (D154); `scores` files are never pruned by `around`. Remaining files are distributed across partitions weighted by `file.total_bytes()`.
2. `StatamSource` scans the remaining files via `StorageIo` (direct `pread` via `FileReader` in local mode; cancellable `ObjectStoreRangeReader` through `read_cache` in S3 mode; `read_threads = 1` on `cetra-query`'s `WorkerPool`), streaming one mini-block at a time through `StatamScanIter` bounded by `Reservation` and `CancelToken`. It applies `ScanSpec` column projection (including 0-column count scans), shredded `MapUtf8` single-key pushdown (`map_keys` via the `map_projections` lattice), mini-block zone-map pruning, Hot/Lazy SBBF bloom-filter pruning (`id_lookups` on `trace_id`, `session_id`, `user_id`, `score_id`), single-disjunct `TS02` trigram substring pushdown, `sort_desc_hint` + `limit` on `traces`/`scores` when unfiltered and untombstoned, late materialization (filtering narrow columns before fetching external `.stpk` payload chunks through `SingleFilePackResolver`), native `Dict`/`Rle`/`Const` pass-through, and `MetadataCache` (caching `StatamFileMeta` and `PackToc`).
3. `MemtableSource` scans the `active` and `frozen` `Batch`es of the same partitions, evaluating filters in-memory into `Selection` masks without copying column buffers.
4. Combined as a `PartitionStream` union (`UnionSource`). Files and batches that a pending deletion may cover apply the **tombstone predicate** (§7) inline via `filter_uncovered`.

### `TracesSource` (exposed as the logical view `traces`)

`TracesSource` reads (a) the partial rows from `traces` Statam files (`StatamSource`) and (b) partials computed from `spans` memtable batches (`MemtableSource`, computed once per batch and cached in `OnceLock`), sorts partitions by `trace_id` via spillable `ExternalSort`, and merges each trace's run of partials with `NativeTraceMerger`: spec 01's rules, written once in Rust and shared with the flush and compaction (D152). Merged rows stream out in bounded chunks as each run ends (D-A1-13). The order rules of spec 01 § Order make results independent of flush order. A SQL `GROUP BY trace_id` is not used: its ordered `first_value` and list-union aggregates cost several microseconds per trace (`docs/research/storage/traces-benchmark.md`).

- *Columns:* only the columns the query projects or filters on are merged, plus the order keys (`trace_id`, `start_time`, `has_root`, `root_span_id`).
- *Pushdown:* only `start_time` ranges and `trace_id` equality / `IN` prune the partial scans. Every other filter evaluates on merged rows after `NativeTraceMerger`: a non-root partial usually has no session or user, and `cost_total > 0.1` can fail on every partial and pass on the merged row.
- *Time window:* when the filter bounds `start_time` to `[a, b]`, partials are scanned on `[a − trace_lookback, b + trace_lookback]` (default `trace_lookback` = 1 h), merged, and the merged `start_time` is then filtered to `[a, b]`. Memtable partials come from every memtable span batch of the project whose time range overlaps that padded window. A trace with partials more than 1 h apart may show up with a late start time; this is documented as a known limit.
- *Memory:* `ExternalSort` holds partials and spills runs to `tmp/query/` past its `FairSpill` share (§ Resource limits). `NativeTraceMerger` bounds its in-flight trace chunk (`max_traces_per_chunk = (pool.cap() / 4096).clamp(8, 1024)`), reserving input, merger, and scratch buffers from the `ScopedPool` and failing with `422 query_too_large` if insufficient (D-A1-13). A range query's memory therefore does not grow with the traces in its window; its time and spill volume do.
- *Duplicates:* a span retried by a client and stored in two flushes is counted twice by the merge until compaction merges the two files (≈ 10 min, §5). Documented.
- *Fast path for "latest N traces" and non-time trace lists* (`TraceListMode`, D153):
  - **Widening (`TraceListMode::Widening`, `ORDER BY start_time DESC, trace_id DESC LIMIT k`).** Phase 1 (rank) reads disjoint slices back from `b`, or from the newest data when `b` is open: 15 min, then up to 1 h, 6 h, 24 h, then the rest of the range (each slice padded by `trace_lookback`). In each slice it merges only the filter's columns, collects matching `(start_time, trace_id)`, and stops after the first slice reaching `k`. Phase 2 (fetch) merges all requested columns for the top `k` traces within their slice windows.
  - **Two-phase rank→fetch (`TraceListMode::TwoPhase`).** For non-time sorts (`cost_total`, `duration_ns`, `total_tokens`), Phase 1 ranks on narrow sort/filter columns only before Phase 2 fetches full columns for the selected page of traces.
  - **Other queries.** Queries with arbitrary expressions, score-function joins, or range-mode overrides run the full merge.
  - **Result.** Exact match with the full merge, except for traces with partials more than `trace_lookback` apart: the known limit above.
- *Single-trace lookup* (`trace_id = X`), in order of preference:
  1. a time hint (`?around=<ts>`, always present in links generated by the UI, API list responses and SDK helpers; passed as `QueryOptions::around`, D154) narrows the search to files overlapping ±1 day;
  2. otherwise, Hot and Lazy SBBF trace-ID bloom filters select candidate files across all of the project's partitions — a few microseconds per file (hot blooms are cached in `StatamFileMeta` (§4); lazy blooms are fetched on demand);
  3. post-MVP, if per-project file counts make (2) slow: either a `trace_index` table (trace_id → min/max day), sorted by `trace_id` and maintained at compaction, as LangWatch and Opik do; or one combined bloom-filter file per partition, laid out so a lookup is one ranged read per partition, as OpenObserve does.

  Within each candidate file, the `trace_id` sort order means mini-block zone maps locate the trace's rows directly.

### `ScoresSource` (logical view `scores`)

`StatamSource` + `MemtableSource` (`created_at`-only pushdown and inline tombstones), then spillable `ExternalSort` by `(score_id ASC, updated_at DESC NULLS FIRST, ingest_time DESC NULLS FIRST)` for per-`score_id` last-writer-wins deduplication (breaking ties by `ingest_time`), then filter out `deleted`. Once a day has been closed (`fully_merged = true` on every file and no memtable batches), `ScoresSource` skips the LWW sort/dedup step.

### Resource limits

- **Memory:** one process-wide `GlobalPool` of `query.max_memory_total` (default 50% of RAM or of the memory limit of the process's cgroup (its systemd unit or container), at most 8 GiB). Each query has a `ScopedPool` over it, capped at `query.max_memory` (default 2 GiB). Buffering operators (`ExternalSort`, `HashAgg`, `HashJoin`, `NativeTraceMerger` sort) spill to `tmp/query/` via `SpillManager`; an operator that cannot spill (or an allocation above the query cap) fails with `422 query_too_large`. Within a query, `FairSpill` gives each spillable consumer at most `(query.max_memory − unspillable_held) / spillable_consumers` and spills past it; unspillable allocations take what remains up to `query.max_memory`, first come first served (D-A1-13). Compaction has its own standalone `GlobalPool` (§5) and is not counted here.
- **Admission:** `AsyncAdmissionPool` (`Storage::query`) atomically takes `query.initial_memory` (default 64 MiB) from `GlobalPool`, waiting asynchronously (`tokio::sync::Notify`) up to `query.admission_wait` (default 5 s) for it, then fails as busy (503). The `ScopedPool` uses those bytes first and grows up to `query.max_memory`; dropping `ProjectQuery` / `QueryContext` (and any streaming export holding `Arc<ScopedPool>`) cancels/drains in-flight CPU workers via `CancelToken` and returns every byte immediately (D-A1-8).
- **Spill:** `SpillManager` writes `cetra-column` LZ4+CRC-32C binary frames to `tmp/query/`, bounded by `Disk`'s `engine_spill_cap` (free disk above `storage.min_free_bytes`, all queries together) and `MAX_MERGE_WAY = 32` open spill FDs per merge pass. At every measurement (§10) the disk monitor sets the limit to the bytes spilled now plus `free − min_free_bytes`. The limit can be too high by at most the bytes spilled during one measurement interval (one second): spill written between the `statvfs` and the read of the spilled total is counted in the total but not in the free space measured before it. Under the threshold no query spills further. A query whose spill would pass the limit, or that gets `ENOSPC`/`EDQUOT` while spilling, fails as too large (422).
- **Parallelism:** process-wide `WorkerPool` with `target_partitions` (default = CPU cores) worker threads and 8 MiB worker stacks (`D-QE-5`), keeping Tokio dedicated to disk/S3/network I/O.
- **Timeouts:** UI 30 s, SQL console 60 s, MCP 30 s.

### S3 read cache

With an S3 object store, every page read is a ranged GET. The S3 store is wrapped by a cache store, so queries, compaction and the media GC all read through it (D-A1-11):
- data files (`tables/…`, both `.statam` and `.stpk` files) are cached in 1 MiB blocks under `cache/<file name>/<block index>`, each ending with a CRC-32C of its bytes; a block that fails its check is fetched again. `cache/<file name>/size` holds the object size as 8 little-endian bytes and their CRC-32C (one HEAD per file, the first time); a `size` file that fails its check drops its directory at open. Media blobs, writes and listings pass through;
- a range read serves the blocks it has and fetches the missing ones in one coalesced request;
- files are immutable, so entries never go stale;
- eviction is LRU by block, within `query.read_cache_bytes` (default 10% of the file system holding `data_dir`, at most 100 GiB; 0 turns it off);
- a delete through the store (GC) or an immediate GDPR rewrite eviction (`evict_read_cache`, §7, D-ST-3) drops both `.statam` and `.stpk` blocks;
- flush and compaction write their fresh outputs into the cache after uploading them, since recent data is what gets read;
- the cache survives a restart: open rebuilds its index from the files, oldest first by modification time;
- it writes nothing while free disk is under `2 × storage.min_free_bytes`, and gives blocks back while free disk is under `storage.min_free_bytes` (§10);
- verification of a suspect file (§10) reads the S3 store directly (`objects.raw_io()`), never the cache, and a file found healthy has its cached `.statam` and `.stpk` blocks dropped, so a bad block cannot outlive the check.

`MetadataCache` is cetra's bounded LRU cache for `StatamFileMeta` and `PackToc` (§4). With a local object store there is no read cache.

The cache is not a durable format (§ 12): a layout change must either make old entries fail their checks or wipe `cache/` at open. The block size is pinned by a test.

## 7. Deletion (GDPR)

### Requests

A request is created by the server in `meta.db` (04); the server's deletion worker resolves its scope, stores it with its cascades, and hands it to storage with `register_deletion` (D-M-3). The storage kinds below are the server's `trace_ids`/`filter` (→ `traces`) and `user`. It has a `kind`, a `created_at` that serves as the **cut-off**, and a resolved scope:

| Kind | Scope stored | Rows deleted |
|---|---|---|
| `traces` | `deletion_traces(trace_id…)`. A filter-based request is resolved to trace IDs by the server's worker, before registration. The set is extended with the **evaluation traces** whose `metadata['cetra.eval.target_trace_id']` is in it (they contain copies of the evaluated data). | `spans` and `scores` rows of those traces with `ingest_time ≤ cut-off` |
| `user` | `user_id`, plus the trace IDs and session IDs the server's worker resolved before registration, plus their evaluation traces | with `ingest_time ≤ cut-off`: `spans` rows of those traces or with that `user_id`; `scores` rows of those traces, with that `user_id`, or with one of those `session_id`s (D-A1-4) |
| `project` | `project_id` | everything in the project (`drop_project`) |
| `org` | `org_id` | every project of the org (`drop_project` for each), plus org-level metadata in `meta.db` |

Session IDs match `scores` only: a session shared with another user keeps that user's spans, and the user's own spans in it are already covered by the user's trace IDs and `user_id` (D-A1-4). `traces` rows follow from the `spans` rows (below).

The cut-off makes the result deterministic: data ingested after the request is new data, not covered by the deletion.

**Handoff.** `register_deletion` is idempotent by deletion id. It stores the request and its resolved scope in `catalog.db` with the next **`deletion_seq`** (1, 2, … across all projects), then publishes the tombstones in a new read state, and returns only after both. A request for a dropped project is stored as already applied, without its scope, and publishes nothing. The server (04 § Deletion requests, D-M-3):
1. creates the request in `meta.db` (status `resolving`);
2. resolves the scope and stores it with the `meta.db` cascades in one transaction;
3. calls `register_deletion`;
4. sets `handed_over_at` (status `pending`).

Rows stay visible until step 3, normally seconds after the request (D13 as amended by D-M-3). At startup the worker resumes every open request: it resolves the ones without a stored scope and registers the others; storage ignores ids it already knows. The server polls `deletion_status` (`pending` or `applied`) to mark its own row `applied`.

### Making data invisible immediately

Pending `traces` and `user` deletions are kept in memory per project as the **tombstone set**: each deletion's `deletion_seq`, cut-off, trace IDs, user and sessions. One function (`cetra_query::tombstone::covered_predicate`) turns a set of deletions into a `cetra_query::Expr` predicate "this row is covered" (D-A1-3); queries, flush and compaction all use it, so the rule exists once:
- `spans`: `ingest_time ≤ cut-off AND (trace_id IN … OR user_id IN …)`;
- `scores`: the same, `OR session_id IN …`;
- `traces` partials: `trace_id IN …` only. A partial carries no per-row ingest time, so a query hides a covered trace's partials whole; the files rewritten by the rewriter recompute their partials from the remaining spans.

Every `IN` term is guarded by `IS NOT NULL` (`scores.trace_id`, `user_id` and `session_id` are nullable), so the predicate is never null: a row with no `user_id`, or a session score with no `trace_id`, is never covered by a term on that column.

Known limits:
- `EXPLAIN` in the SQL console prints the predicate. The trace ids of a pending deletion show until it is applied (accepted); user and session ids print only as a count, `user_id IN (<n redacted>)`, so the plan never names an erased person (D-A1-3).
- For a `user` deletion, the `traces` view hides partials by the resolved trace IDs only; the server resolves every trace with a span of the user ingested up to the cut-off.
- A row stamped with `ingest_time ≤ cut-off` but appended after its deletion is applied is neither dropped nor hidden. The server stamps `ingest_time` at receipt, so this needs a request in flight for longer than the deletion takes to apply.

A deletion may cover rows of a `spans` or `scores` file only when its `deletion_seq` is above the file's `deletions_applied_seq` and its cut-off is at or after the file's `min_ingest_time`; of a `traces` file, only when its `deletion_seq` is above the file's `deletions_applied_seq` (a `traces` file's `min_ingest_time` is the smallest per-trace *latest* ingest time, so it can't bound a cut-off); of a memtable batch, only when its cut-off is at or after the batch's `min_ingest_time`. The query path filters only those files and batches:
- a `traces` file's gate is built from the deletions with `deletion_seq` above its `deletions_applied_seq`; an applied deletion never hides a later partial;
- memtable batches are filtered in memory when the read is planned (with fresh `traces` partials computed from what remains);
- files are scanned with the predicate's columns added to the projection, and `StatamSource` / `MemtableSource` evaluates `filter_uncovered` inline via `Selection` (updating the active-row mask without copying column buffers), before restoring the requested columns. Other files are scanned as usual.

`project` and `org` deletions are immediate: `drop_project` marks the project dropped in `catalog.db` and removes it from every new read state in the same commit; the server returns 404 for it.

### Cascades, in the worker's scope transaction (`meta.db`)

The server stores a resolved scope and its cascades in one transaction, before it registers the deletion (D-M-3):
- Dataset items whose `source_trace_id` is in scope are hard-deleted, all versions (track J).
- `experiment_items` referencing those traces, or snapshotting a deleted item revision, are deleted (track J, D-J-9); `eval_jobs` referencing them are deleted unless `running` (D-M-6).
- Issue candidates of the deleted traces (track IC).
- A running judge job, and any judge run that loaded its target before the deletion, is stopped by the server's 24-hour deletion guard before it commits a judge trace or a score; a copy it committed just before the guard saw the deletion has an `ingest_time` after the cut-off, so the executor stores a follow-up `trace_ids` deletion of the judge trace and the target (D-M-6).
- `alert_events` hold only aggregates; they are pruned after 90 days with their deliveries (D-K-10).

### Physical erasure: the deletion rewriter

Every file records `deletions_applied_seq`: no row in scope of any deletion with `deletion_seq` up to that number remains in it.
- A flush sets it to the highest `deletion_seq` known when it started, since it drops every row the pending deletions cover (§2). A flush left with no row commits only its `flushed_seq`.
- Compaction and the rewriter set it to the highest `deletion_seq` known when the job started, since they apply all of those. A `traces` output gets the value of its `spans` output.

After each compaction pass, the compaction task runs a **deletion pass** (D-A1-5):

1. **Candidates:** the active `spans` and `scores` files that a pending deletion may cover (rule above), skipping files claimed by a running job. The pass claims them.
2. **Check:** a scan of the file's key columns through `StatamSource` with the tombstone predicate and early exit after 1 match (`ControlFlow::Stop` / `Limit(1)`), so mini-block zone maps and SBBF bloom filters (`trace_id`, plus `user_id` and `session_id` for `spans`) skip row groups.
   - A covered row: the file is **rewritten** without its covered rows by a compaction job of kind `rewrite`: same level, same `fully_merged`, the `traces` file derived from it recomputed (§3), committed like a compaction (§4). A file left with no row is removed with its `traces` file (and unused staged paths are unstaged via `CatalogEdit.unstage`) and nothing replaces it.
     - For L1 `spans` files with a sibling `.stpk` pack, `rewrite` runs `gdpr_rewrite_single_file_streaming` in O(1 RG) + O(1 frame) peak RAM (D-ST-3): Pass A scans narrow columns and `PayloadRefs` to mark surviving rows and live chunks; the pack rewrite streams one frame at a time, copying 100%-live non-dict frames verbatim without decompression, unconditionally discarding any legacy `DictSection`, recompressing mixed frames with live chunks only, and dropping 0%-live frames (guaranteeing 0 bytes of erased payloads remain in either rewritten file); Pass B streams surviving rows with remapped chunk refs into the new `L1Compact` `.statam` file while buffering only the trailing `trace_id` group across RG boundaries to compute exact derived `traces` partials without splitting straddling traces. The new `.stpk` pack (if any chunks survive) is published before `.statam` and `traces`.
     - Immediately after any `rewrite` commit succeeds, the replaced `.statam` and `.stpk` entries are evicted from `ReadCache` (`objects.evict_read_cache`) and `MetadataCache` (`invalidate_file` / `invalidate_pack`) (D-ST-3).
   - No covered row: its `deletions_applied_seq` is raised to the known sequence in one catalog edit, without a rewrite.
   - **Quarantined files** (§10) can't be rewritten: the quarantined `spans` and `scores` files of the project that a pending deletion may cover (same rule) are marked `obsolete`, with the quarantined `traces` files derived from them, and GC deletes them.
3. **Applied:** a deletion is applied once:
   - every active `spans` and `scores` file of the project with `min_ingest_time ≤ cut-off` has `deletions_applied_seq ≥` its `deletion_seq` (`traces` files follow their source `spans` files, §3);
   - no memtable batch of the project has `min_ingest_time ≤ cut-off`. A batch that held covered rows is dropped by the next flush, which writes none of them.

   Both are read from one published read state, so a flush committing in between cannot make a deletion look applied. Then `status = 'applied'`, `applied_at` is set, its scope rows (`deletion_traces`, `deletion_users`, `deletion_sessions`) are deleted, its tombstone entries are removed, and media GC runs for the project (§9). An applied request keeps only its id, project, kind, cut-off, sequence and dates; the audit record is the server's `meta.db` row, which keeps the scope (D-M-8).

Compaction and the rewriter never run on the same file (claims, §4), and a compaction that started before a deletion records an older `deletions_applied_seq`, so its outputs remain candidates. Deleted rows cannot come back through a merge.

**Guarantee:** deleted rows are hidden from reads within seconds and gone from live Statam files (and sibling `.stpk` packs) within `deletion.max_delay` (default 24 h), with retired entries evicted from `ReadCache` and `MetadataCache` immediately on rewrite commit (D-ST-3). The rewritten files' old `.statam` and `.stpk` objects are marked `obsolete` and stay in the object store until GC removes them, `storage.gc.grace` later (§5; default 5 min, at most 30 days), so the bytes are gone from storage within `deletion.max_delay` + `gc.grace` (D181). Project and org drops and retention follow the same bound. A pending deletion older than that is logged once as a warning. The WAL holds a deleted row only until the end of the flush cycle after it was written (§1, D-A1-14), so the row ages out with the flush checkpoints (minutes at low traffic), not at the segment size. A clean shutdown does not rotate, because the writers stop before the final flush: the drained segment then waits for the first flush after the next start. The meaning of GDPR deletion for backups is covered in 12 §Backups.

### Project drop

`drop_project(project)` (D-A1-7):
1. waits for appends in flight, and holds new ones, while it commits;
2. commits one edit that obsoletes every file of the project in all three tables (quarantined files included), records the project as `dropping` in `projects`, removes its retention watermark, and marks its pending deletions applied and erases their scope rows;
3. publishes the read state without the project: its queries return empty tables, and `append_*` for it fail with `ProjectDropped`;
4. deletes its media objects and `media` rows, then sets the project `dropped`.

GC removes the obsolete files like any others (§5), so no listing of the `project=` prefixes is needed. If step 4 fails or the process stops, the GC task finishes it. Rows of the project still in the WAL or memtable are dropped by the next flush, and the committer refuses any file of a dropped project (§4). A `project_id` is never reused, so a dropped project is never re-created.

## 8. Retention

Each project has `retention_days` in `meta.db` (null means keep forever). The server calls `apply_retention(project, keep_from = today − effective days)` hourly, and once when a `PATCH` sets retention. The effective days are the shorter of the project's `retention_days` and its org's plan cap (data-access window + 30 days, D175), when either is set (D-M-10, D-M-15). Storage then:
- commits one catalog edit that obsoletes every file of the project in days before the watermark, across `spans`, `traces` and `scores`, quarantined files included (no file is rewritten, because partitions are per day);
- records the watermark in `retention_watermarks`, in the same transaction. The watermark is the later of the stored one and `keep_from`: it never moves back.

From then on, a flush drops rows of that project older than the watermark, queries don't show memtable rows older than it, and the committer refuses a file older than it (§4), so late rows cannot recreate a dropped day. Memtable `spans` are keyed by each row's own day and files by the anchor day, so a span next to midnight at the watermark can be hidden in the memtable and kept by the flush, or the reverse; the file is what counts. Retention is day-granular. Since a trace lives in one anchor day (§ Trace anchor), it is dropped as a whole. Judge traces and scores are dropped by their own day, not their target's: an evaluation or a score written later about an old trace lives at most the retention plus the target's age when it was written, and can outlive its target (its trace link then leads nowhere). A deletion, unlike retention, follows the target to its judge traces and scores (§7, D-M-6, D-M-10).

## 9. Media

Media blobs live under `media/<project_id>/<sha256>` (64 lowercase hex) in the object store; their index is the `media` table of `catalog.db`.

`put_media` checks that the SHA-256 of the bytes matches the given hash, and fails otherwise (D-A1-6). For a hash already stored it only refreshes `last_seen_at`. Otherwise it records the path in `staged_objects`, writes the blob durably (local: temp file, `sync_all`, rename, directory fsync; S3: a plain `PutObject`, since the name is the content's hash and an overwrite writes the same bytes), then inserts the `media` row and removes the staged row in one transaction. Staged-object GC deletes an object left without a row, and leaves alone a staged path whose `media` row exists. `get_media` reads a blob, whole or a byte range. Each row keeps `newest_span_start`, the newest `start_time` of the spans that referenced the blob (raised by every `put_media`); `get_media` with an org's `visible_from` answers not found when it is older (D-P-21).

**GC** runs daily, and for one project right after one of its deletions is applied (§7). For each project, it reads only the `media_refs` column of `spans` (a cheap projection, through the same read path as queries, so rows hidden by tombstones don't count) to build the set of live hashes. It then deletes the objects and `media` rows that are absent from the set and whose `last_seen_at` is more than 1 h old. The age guard covers a span appended just after its blob; since every `put_media` refreshes `last_seen_at`, a blob reused by new spans is never collected under them (D-A1-6).

A dropped project's media is deleted by `drop_project` (§7).

## 10. Failure handling

| Situation | Behaviour |
|---|---|
| Free disk under `storage.min_free_bytes` (default 2 GiB) | Free disk is measured every second: the least free of the file systems holding `data_dir` and, in local mode, the object store root. Below the threshold `append_spans`, `append_scores` and `put_media` fail with `DiskFull` (503). Flush, compaction, GC, retention and deletions keep running: they reclaim space. A compaction job reserves its input size before it starts and is deferred to a later pass when it doesn't fit (§5); flushes don't reserve (D-A1-9); query spill stops at the threshold (§6) |
| WAL write or fsync error | pending and new appends fail with `WalFailed` (503), health reports it, restart required (§1) |
| Local object fsync error (publish, delete, startup walk) | the job fails; every later local object write and delete fails with `SyncFailed`, so every flush attempt fails on each retry, and the compaction and deletion passes do nothing (an empty report) until a restart; health reports it (`/readyz` 503); restart required. Reads continue (D-R-6) |
| `catalog.db` I/O error at a commit | the committer stops; every flush attempt fails on each retry, the compaction and deletion passes do nothing (an empty report) until a restart, and retention, drop and GC runs fail; appends continue until backpressure; health reports it (`/readyz` 503); restart required (D-R-6) |
| Object store errors during flush | retry after 1 s, doubling up to 60 s (§2), while the memtable keeps the data and backpressure applies past the hard limit |
| Object store errors at open | The anchor rebuild reads recent `traces` files; a store error, after the client's retries, fails the open. A later open retries. Acknowledged data stays in the WAL |
| Ambiguous upload (error reported, object may exist) | retried under a new name; the old one is collected from `staged_objects` (§2); a failed multipart upload is aborted (§2). Uploads no abort reached are left to the bucket's lifecycle rule (`https://cetra.sh/docs/self-hosting/s3/`) |
| Crash | WAL replay before ingestion opens; staged objects are GC'd |
| Corrupted Statam file or pack | A panic in the Statam decoder during any of these reads is caught at our scan (`src/unwind.rs`) and handled as a decode error; transient object-store errors (`StatamError::Store`, with privacy-safe static reasons) are never quarantined (`Verdict::Unavailable`). A read that fails with a corrupt/decode error (`StatamError::is_corrupt()`) makes the files it read **suspects**: compaction and rewrite jobs (their inputs), the deletion pass's check scan, the anchor rebuild at open (which skips the file instead of failing), the media GC and queries (every file the query scanned). After each deletion pass, a verify pass runs `verify_statam_file` on each suspect whole through `objects.raw_io()` (bypassing `ReadCache`): checks `HEAD` size of the `.statam` file (and sibling `.stpk` when present), checks catalog `row_count` and `TableKind`, and runs `StatamReader::verify_full_with_pack` (verifying `.statam` header/postscript/RG/block/lazy-index CRCs and decode, plus when a pack is present, `PackHeader`/`PackPostscript` IDs, every trailer section CRC, every `.stpk` frame decompression + CRC + chunk bounds, and every `PayloadRefRow::Chunks` reference; or verifying zero external refs when `pack` is `None`). A store error puts the suspect back in the queue; a healthy file drops its cached `.statam` and `.stpk` blocks (`drop_cached_blocks`) and isn't re-verified for an hour; a decode/CRC/ref error, wrong size or row count, or missing `.statam`/`.stpk` object makes it **corrupt**, logged as an error. A corrupt `spans` file (or corrupt sibling `.stpk` pack) is quarantined with its derived `traces` file, a corrupt `scores` file alone: state `quarantined`, out of every snapshot, `MetadataCache` invalidated for both file and pack, objects kept for inspection, its rows lost. A corrupt `traces` file is rebuilt from its `spans` file by a rewrite job instead; nothing is lost. Quarantined files become `obsolete` (and GC deletes them) when retention drops their day, when their project is dropped, or when a pending deletion may cover them (§7). There is no periodic scrub (D-A1-10) |
| The disk fills before the monitor sees it (`ENOSPC`) | WAL: `WalFailed`, final (§1). Flush: retried with backoff (§2). Compaction: the job fails, and a later pass redoes it. `put_media`: the call fails, and GC collects the staged row (§9). Read cache: the block is not cached. Query spill: too large (422). A failed write removes its temp file. Once space is free, the next attempt succeeds. Inodes are not monitored: running out of them behaves the same way |
| Out of file descriptors (`EMFILE`) | The step that opened the file fails as for an I/O error: a query fails, and a flush or compaction is retried. *(Provisional, pending the coordinator's decision on WAL rotation: until it is recorded, an append is refused when a WAL segment cannot be created, then `WalFailed`, §1. Once it is recorded, the writer keeps the current segment instead.)* Nothing is quarantined |
| Memory limit | Below the documented minimum (§ Resource needs) the kernel can kill the process. At or above it, the budgets refuse work instead: `Backpressure` (503), `Busy` (503), too large (422) |

The server reads free disk (`StorageHealth::disk_low`, `disk_free_bytes`) for `/readyz` and `/metrics`. It probes the object store by HEADing the marker `_probe/readyz` and, if that fails, writing it (`Storage::probe_objects`, D-N-36; no `s3:ListBucket` needed). It reads monotonic counters (`Storage::stats`) for `/metrics` (N M3, D-N-30, D-N-31).

The model test (`crates/storage/src/model_tests/`, track R M1) checks every read against an in-memory model through restarts and crashes.

### Resource needs

- **Disk:** `storage.min_free_bytes` must cover at least one second of the peak write rate (WAL plus flush and compaction outputs), because free disk is measured once a second. The default 2 GiB covers about 2 GB/s. *Measured:* with `min_free_bytes` at 16 MiB, appends were refused with `DiskFull` before the disk filled, and no WAL failure followed (`disk_full::appends_stop_at_min_free_before_the_disk_fills`). With `min_free_bytes` at 8 MiB, a read-cache run kept the lowest free disk at 15.8 MiB, just under `2 × min_free_bytes` (R5.8).
- **Memory:** the budgets add up as:
  - `2 × memtable.hard_limit` (spans and scores, each table on its own);
  - plus the copy a flush makes while sorting (up to `flush.max_bytes` per table, so `2 × flush.max_bytes`);
  - plus `compaction.max_memory`;
  - plus `query.metadata_cache_bytes`;
  - plus the query pool (`query.max_memory_total`, by default half the limit);
  - plus about 200 MiB of base.

  *Measured (R5.8, pre-QE):* the defaults profile passes at 8 GiB (peak RSS 7.40 GiB) and is killed below it, between 7.75 and 8 GiB. The small-machine profile is `memtable.hard_limit` 128 MiB; `flush.max_bytes` 32 MiB with `upload_concurrency` 2; `compaction.max_memory` 128 MiB with `compaction.threads` 1; `query.max_memory_total` 256 MiB, `query.max_memory` 128 MiB, `query.initial_memory` 16 MiB; `query.metadata_cache_bytes` 64 MiB; `query.target_partitions` 2. The small profile needs about 4 to 4.5 GiB with the glibc allocator: it passed at 4 GiB and was killed at 2 GiB in the first runs, and a rerun on the VM (2026-11-22) was killed at 4 GiB and passed at 4.25 GiB (peak RSS 4.45 GB). Allocator tuning (`MALLOC_ARENA_MAX=2`, 1 MiB trim and mmap thresholds) lowers the minimum to at most 2 GiB. The accounted budgets are a small part of that: the memtable stays at or below 130 MiB and the query pool at about 170 MiB. Most resident memory is outside every budget and grows with ingest. The shipped binary uses mimalloc, which is not measured on this workload (provisional). The unaccounted memory is not located yet (M5-7, `docs/research/storage/hardening.md`). In `cetra-query`, `ExternalSort` participates in `FairSpill` sharing and spills cleanly without a fixed 10 MiB-per-partition reservation. The budgets are not sized from the machine: `Storage::open` logs a warning when they add up to more than the memory limit it detects. The server adds `ingest.max_inflight_bytes` and its own needs (spec 12).
- **File descriptors:** the engine holds one descriptor for `LOCK`, three for `catalog.db` (the database, `-wal`, `-shm`), one open segment per WAL table (plus one for a moment when a segment is created), and, while they run, up to `flush.upload_concurrency` temp files per table and one per compaction job. A query holds a descriptor per spill file while it is written, and a spill merge opens all of a sort's spill files at once. *Measured (R5.8, soft limit 65,536):* the baseline is 16 to 17 descriptors and the peak under load is 189, with a 128 MiB query cap; 166 of them are spill files, since a merge opens all of a sort's spill files at once. Limits of 256, 128, 64 and 32 run without a panic or a wedge, with clean query and compaction errors; at 32 the WAL fails and a reopen recovers. Opening fails cleanly at 8. The recommended soft limit for the storage is at least 4,096, or the measured peak × 4, whichever is larger. The server's sockets come on top.
- **Tested by** `just resources` and `just resources-memory` (track R M5).

## Durability

Every write path, what it syncs and in what order. "fsync dir" is an fsync of the directory that holds the entry. A failed fsync is fatal for its component (D-R-6), except where the table says otherwise and why. The shim tests run each path that has a crash test under a lossy crash (`crates/storage/src/fs/lossy.rs`, track R M2); a path marked "not tested" has none. LazyFS runs the crash tests on the real binary (`tools/lazyfs/`), which is pending.

| Write path | Code | Steps, in order | On fsync failure | Test |
|---|---|---|---|---|
| Data directory layout | `lib.rs` `prepare_layout`, `ensure_dir` | `mkdir` (or it exists) → fsync parent, at every open | open fails | `the_data_dir_layout_is_durable_after_a_crash_before_its_sync` |
| Lock | `lib.rs` `lock_dir` | create `LOCK` → `flock` → fsync data dir | open fails | `a_second_open_is_locked_until_shutdown` (the lock; the fsync is not tested) |
| WAL segment creation | `wal/segment.rs` `SegmentWriter::create` | create the file, write the header, `fdatasync` the file, `fsync` `wal/<table>/` | at open: open fails; at rotation: the writer stops | `a_created_segment_keeps_its_header_through_a_lossy_crash`, `a_lossy_crash_after_an_interval_rotation_reopens`, model (`LossyCrash`), crash children |
| WAL group, `"always"` | `wal/writer.rs` `IoJob::commit`, `SegmentWriter::sync` | `write_all` → `fdatasync` → memtable → publish → ack | the writer stops; the group stays as written; `WalFailed` | `a_failed_sync_leaves_the_group_and_does_not_sync_again`, `a_failed_sync_group_is_lost_on_power_loss`, model (`LossyCrash`) |
| WAL group, interval | `wal/writer.rs` `IoJob::commit`, `run` | `write_all` → ack. `fdatasync` at the first group after the interval, at the idle timer, at shutdown, and before rotation | as above | `interval_rotation_syncs_the_old_segment`, `storage_matches_the_model_in_interval_mode` |
| WAL rotation | `wal/writer.rs` `IoJob::commit` | `fdatasync` the old segment (interval mode) → create the new one (fsync dir) | the writer stops | `interval_rotation_syncs_the_old_segment` |
| WAL replay | `wal/replay.rs` `recover_table`, `truncate_tail` | torn tail: `set_len` → fsync the file. A segment shorter than the header that is a prefix of it holds no records: truncated to empty when last, skipped when not. The replayed segment is fsynced at open | open fails | `a_torn_tail_truncates_to_the_last_full_record`, `a_torn_header_is_a_torn_creation`, `reopen_syncs_the_replayed_segment` |
| Discarded group | `wal/writer.rs` `discard_unacked` | `set_len(mark)` → fsync the file; unlink the new segment → fsync dir | logged; the writer is stopping anyway | `a_discarded_segment_stays_removed_after_a_lossy_crash` |
| WAL truncation | `wal/mod.rs` `truncate_segments`, `flush.rs` `truncate_flushed` | unlink covered segments → fsync dir | logged and **retried** by the next flush. Not fatal, and why (below) | which segments go: `truncation_keeps_the_newest_segment_and_unflushed_records`. The retry: not tested |
| Local object publish (flush, compaction, rewrite, media) | `objects.rs` `put_local`, `publish_local` | each new directory: fsync parent → temp file → write → fsync file → rename → fsync dir → the caller's catalog commit | the store stops (`SyncFailed`) | `a_failed_object_fsync_refuses_later_writes_and_deletes`, `a_failed_sync_of_a_new_day_directory_stops_the_store`, `an_outside_object_root_under_an_unsynced_ancestor_survives_a_lossy_crash`, crash children |
| Local object delete (GC of obsolete, staged and media objects; orphans) | `objects.rs` `delete_local` | unlink → fsync dir → the caller's row delete | the store stops | `a_deleted_object_stays_deleted_after_a_lossy_crash`, `gc_leaves_no_orphan_after_a_lossy_crash`, `a_failed_delete_sync_stops_the_store` |
| Object tree at open | `objects.rs` `remove_temp_files` | unlink temp files → fsync the directories, then the root | open fails | `an_object_in_a_directory_left_unsynced_survives_a_lossy_crash` |
| `catalog.db` | `catalog/committer.rs`, `catalog/db.rs` | SQLite in WAL mode, `synchronous=FULL`. Every commit point goes through `commit_point` | the committer stops until restart (D-R-6) | `every_commit_point_stops_the_committer_on_an_fsync_error`, `fatal_io_codes_stop_the_committer_and_the_rest_stay_retryable`, `an_fsync_error_stops_the_committer_until_reopen`; LazyFS (pending) |
| S3 objects | `objects.rs` remote writes | the PUT or `complete` returned → catalog commit | n/a: no local fsync | `tests/it/s3.rs` (a real S3-compatible server, `CETRA_TEST_S3_ENDPOINT`) |
| Read cache | `read_cache.rs` | no fsync. Blocks and `size` carry a CRC-32C; a bad one is dropped and fetched again | n/a | `a_torn_size_file_never_serves_short_ranges`, `a_corrupt_block_is_fetched_again` |
| Scratch: query spill, compaction temp | `SpillManager` (`tmp/query/`); `create_output` | no fsync; leftover spill files are removed at open | n/a | `open_removes_leftover_spill_directories` |
| Catalog snapshot (N B3) | `snapshot_catalog` (`snapshot.rs`) | `VACUUM INTO` (SQLite) | the backup fails | N's tests |
| Backup object copies (N B4) | `copy_objects` (`snapshot.rs`; calls `crate::fs`, unrecorded in lossy mode because `dest` is outside the data directory) | hard link, or copy → fsync file; at the end, fsync every directory of `dest` | the backup fails | N's tests |
| Pre-migration copy (N B9) | `catalog/db.rs` | `VACUUM INTO <tmp>` → fsync the copy → rename → fsync dir (all through `crate::fs`) → migrations | open fails | none yet: `a_pre_migration_copy_survives_a_lossy_crash` is not written |

The N rows (backups, pre-migration copy) merged with their file calls routed through `crate::fs`, as R2.1 requires. Their lossy-crash tests are not written yet.

**Why a failed WAL truncation is retried, not fatal.** Truncation deletes only segments whose every record is at or below `flushed_seq`, and the catalog has already advanced `flushed_seq` when it runs (§1). If the directory fsync fails, the segments may come back after a power loss. They then hold only records that replay skips, and every non-last segment was fsynced before its successor was created (§1), so it is intact. The next flush retries the truncation, and nothing is lost. The retry is not covered by a test.

The WAL reader rejects any corrupt `CBM1` frame or torn tail without panicking (§1). Both the WAL framing and `cetra-column::frame` decoders are fuzzed (track R M3, D-QE-7, `fuzz/`).

## 11. Toward multi-node

Nothing here assumes a single node, apart from the WAL and memtable living in the same process as the querier. The planned split:

- **Ingesters** own the WAL and memtable, sharded by project.
- **Queriers** are stateless; they read files from S3 and ask ingesters for their memtable snapshot as `cetra-column` `CBM1` frames over streaming RPC.
- **The catalog** moves to Postgres.

File formats and paths stay unchanged.

## Capacity sanity check

At 50k spans/s with an average normalized span of about 3 KiB (LLM spans 10–50 KiB, others under 1 KiB):
- Ingest is about 150 MiB/s of uncompressed `cetra-column` batches; with LZ4 `CBM1` framing the WAL writes roughly a third of that, which fits easily on NVMe.
- The memtable flushes every ~1.7 s at 256 MiB. With that rate, raise `flush.max_bytes` to 1–2 GiB to get bigger L0 files.
- With zstd at roughly 5–8× on JSON-heavy text, compressed output is about 20–30 MiB/s, or 2–2.5 TiB/day.

Sustained use at this rate needs S3 and a bigger box. The target box (8 cores, 32 GiB) is sized for bursts at that rate and sustained ingest about an order of magnitude lower.

## 12. Format versions (D-R-12, D-R-26 to D-R-32)

From the first public release (D185), a data directory written by any release must open with every later one, and a directory written by a newer binary must be refused by an older one without being changed. Before that release there is no constraint.

**One number.** `FORMAT_VERSION` (`crates/storage/src/format.rs`, a `u16`, 1 at the first release) is the format version of the whole data directory. It is stored in three places:

| Surface | Where the number is | A reader that finds a newer one |
|---|---|---|
| Data directory | `catalog.db` `PRAGMA user_version`: the highest format any file of the directory may use. Written before a binary first writes anything in a newer format, never lowered. | `open` fails with `FormatTooNew` before it migrates, copies or writes anything. |
| WAL | an 8-byte header at the start of every segment (§1) | `open` fails (`WalError::Header`); the segment is not truncated, and `--wal-skip-corrupt` does not apply. |
| Statam | wire format version in `FileHeader`/`Postscript` (and `PackHeader`/`PackPostscript` for `.stpk`) (§3) | `StatamReader` refuses `UnsupportedVersion`; the directory marker comes first at open. Checked by the golden tests. |

The S3 read cache (§6) and `tmp/` hold no durable data and carry no version. `FORMAT_VERSION` remains `1` pre-release after the Statam cut-over (D-ST-4).

**What is a format change.** Not one: a new nullable Statam column (§3), a `catalog.db` migration that only adds (`CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE … ADD COLUMN` that is nullable or has a default), a new config key. Every other change is one: a new or changed WAL `kind`, segment header, compression or payload schema; a changed Statam column type, sort order, wire encoding or `.statam`/`.stpk` layout; a renamed or re-typed catalog column, a dropped table, an `UPDATE` of existing rows, a new value in a TEXT enum column (`data_files.state`, `table_name`, `deletions.kind`, `projects.state`); a changed object path layout; a changed checksum; a changed read-cache block size.

A released migration file is never edited; the golden catalogs enforce it.

**A release that changes a format** bumps `FORMAT_VERSION`, keeps a reader for what the previous release wrote (older WAL segments are replayed before the new format is written; older Statam files and `.stpk` packs are read, not rewritten at upgrade; a catalog migration runs at open), has tests that open the previous releases' golden directories, and adds a line under `### Upgrade notes` in `CHANGELOG.md`.

**Opening.** `catalog.db` is opened first. Its `user_version`: above `FORMAT_VERSION` is refused; `0` with an empty catalog is a new directory and is stamped; `0` with data is a pre-release directory and is refused (`PreFormat`). A migration the binary does not know (`_sqlx_migrations` has a version it lacks) is refused (`SchemaTooNew`). These checks run on a read-only connection, followed by a read of every WAL segment's header (§ 1), before anything is migrated, stamped, replayed or written: a refused open changes no existing file's content (`open` may create missing directories and `LOCK`). Downgrading is not supported: the way back is the backup taken before the upgrade (12 § Upgrades).

**Golden directories.** `fixtures/storage/golden/<version>/` is a small data directory written by the release's own build (`just golden-dir <version>`), with `expected.json`: the answers of the public reads. A test opens a copy of every one with the current code and compares; none is committed before the first stable release, and until then the same test generates a directory into a temp dir and checks it. `fixtures/storage/golden/RELEASES` lists the released versions. Each stable release adds its directory and keeps the old ones: every 0.x directory until 1.0, every release's directory from 1.0 on (D-R-12). `just check-golden` fails a change that deletes a released directory.

**At a release.**
- `just release-prep X.Y.Z`, then `just golden-dir X.Y.Z` (writes `fixtures/storage/golden/X.Y.Z/` and appends to `RELEASES`).
- Commit both with `Release X.Y.Z`.
- A pre-release (`X.Y.Z-rc.N`) has no directory.

**Amending a released `expected.json`.** A released directory and its `expected.json` are never rewritten, with one exception: a released build's answer is proven wrong (a bug in a read), so the recorded value was wrong. The amendment is its own commit, explains the bug, and adds a line under `### Upgrade notes` in `CHANGELOG.md` when the answer a user sees changes.

**Not covered here:** `meta.db`, `secret.key`, `admin.sock` and the backup `manifest.json`. They belong to the server (04) and to online backups (12).
