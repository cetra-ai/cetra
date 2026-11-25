Status: approved (2026-11-25). M1 complete; M2 plan under cold review.

# QE · Custom query engine and columnar format — design

Track **QE** replaces Apache DataFusion and Apache Arrow in `cetra` with our own pure-Rust columnar in-memory format (`cetra-column`) and our own analytical query engine (`cetra-query`), keeping `sqlparser` for SQL and filter parsing.

The track is driven by three priorities, in order:

1. **Owning the stack:** one cohesive Rust query and columnar stack tailored to LLM observability, with no third-party planner surprises and no DataFusion upgrade churn (and removing the remaining `arrow` / `parquet` / `object_store` lockstep once Strata replaces Parquet storage).
2. **Latency and memory control:** sub-5 ms floor on small queries, shared-scan multi-field facets and dashboard widgets, two-phase non-time trace lists, true selection-vector late materialization, exact byte-level memory accounting across every operator, and sub-millisecond query cancellation.
3. **Compile time and binary size:** removing DataFusion (~30+ crates) completely and removing Arrow from `cetra-core`, `cetra-column`, `cetra-query`, `cetra-otel`, `cetra-server` and `cetra-cli` (and removing Parquet storage reads/writes once Strata clears its go/no-go).

DataFusion remains in the workspace during M1–M6 as a **differential test oracle** — every new query path is checked against the existing implementation before DataFusion is deleted in M7. Every milestone ends with `just check` green.

**Read first:**
- `AGENTS.md` (rules: no `unsafe`, no compatibility layers, crate dependency graph, privacy, tests).
- `docs/mvp-decisions.md`: D3, D7, D23, D30, D31, D39, D48, D52, D60, D62, D83–D101, D152–D154, D-A1-3, D-A1-8, D-A1-13, D-A2-1–D-A2-5, D-AQ-8, D-C-13, D-C-16, D-C-24, D-H-8, D-J-14–D-J-15, D-P-21, D-P-47.
- Specs:
  - `README.md` (cross-cutting conventions);
  - 01 (data model: `spans`, `traces`, `scores`, `sessions`);
  - 03 (storage engine: WAL, memtable, files, catalog, compaction, query path, deletion, media GC, resource limits, read cache, metadata cache, format versions);
  - 05 (query: views, default exclusions, filter DSL, full-text search, SQL console sandbox, parameters, dashboard widgets, facets, trace/span APIs, errors);
  - 07 (alert queries);
  - 09 (read, SQL, export, schema, MCP and CLI contracts);
  - 12 (configuration and operations).
- `docs/research/query-layer.md`, `docs/research/storage/benchmarks.md`, and `docs/research/file-format/README.md`.

This document holds the goals, architecture, contracts, decisions, open questions and milestones. Each milestone gets its own plan, written just before it starts from the code that exists then.

---

## Goals

1. **Own the columnar data plane (`cetra-column`).** Replace Arrow `RecordBatch`es across `cetra` with a small, zero-`unsafe`, `tokio`-free, pure-Rust columnar crate covering every type `cetra`'s storage and Shape B SQL need, with encoded column representations (`Flat`, `Const`, `Dict`, `Rle`), selection vectors, synchronous hierarchical `MemoryPool` accounting (supporting `FairSpill` and separate compaction budgets), `CancelToken` propagation, and a bounded, checksummed binary frame format for the WAL and spill files.
2. **Format-agnostic storage scan boundary (`ScanSource` / `ScanSpec`).** Decouple query execution and storage maintenance from the on-disk file format so the new engine runs on `cetra`'s existing Parquet files today (with our own bounded Parquet metadata cache honoring `storage.query.metadata_cache_bytes`) and plugs directly into **Strata** once Strata passes its workload evaluation, with zero changes to operators, reads or the SQL planner.
3. **Push-based, memory-bounded execution engine (`cetra-query`).** Replace DataFusion's async pull streams with DuckDB-style synchronous push pipelines on a bounded CPU worker pool (leaving Tokio strictly for network, disk and S3 I/O), with out-of-core spilling on buffering operators (`Sort`, `HashAgg`, `HashJoin`, `TraceMerger`) and sub-millisecond cooperative cancellation that drains all workers before releasing reservations.
4. **Faster, cheaper product reads and storage maintenance.**
   - Shared-scan multi-field `POST /facets` (one scan computes all values, list-values, map-keys and histogram statistics; at most one extra narrow scan for exact histogram bucket counts — down from up to 40 scans);
   - Shared-scan multi-measure `POST /dashboards/query` widgets (one scan per period for non-breakdown widgets; two bounded scans for top-K + `other` breakdown widgets, plus the traces semi-join when a dashboard filter applies to `spans`/`scores`);
   - Two-phase `trace_list` for non-time sorts (`cost_total`, `duration_ns`, `total_tokens`), ranking on narrow columns before fetching full rows;
   - Sub-5 ms floor on point lookups, recent lists and `trace_refresh` / `span_refresh` (no SQL session/analyzer/optimizer setup per read);
   - Native, DataFusion-free implementations of compaction merge/slicing, GDPR deletion candidate checking and rewriting (sharing one tombstone predicate with queries and flushes, D-A1-3), media GC live-set scans, and corrupt-file panic/error classification (`unwind` / `verify`).
5. **Curated analytical SQL ("Shape B") for the SQL console and MCP.** Preserve 99% of real LLM-observability SQL queries (including all shipped UI examples, docs queries, alert queries and internal server SQL queries), saved queries with `{name:Type}` parameters, time-window and `visible_from` enforcement, strict multi-tenant sandboxing, GDPR-redacted `EXPLAIN`, and streaming exports (JSON, CSV, NDJSON, and `.parquet` export without `arrow` in the query engine).
6. **Delete DataFusion completely, and isolate Parquet/Arrow to `cetra-storage`.** By the end of this track, `datafusion` and `datafusion-datasource` are removed from the workspace; `cetra-core`, `cetra-column`, `cetra-query`, `cetra-otel`, `cetra-server` and `cetra-cli` (non-test dependencies) have zero `arrow` or `parquet` dependencies; `.parquet` export runs on `parquet`'s low-level writer with `zstd` only (no `arrow` feature); and Parquet/Arrow storage reads and writes live strictly inside `cetra-storage`'s file-format adapter until Strata replaces them.

## Non-goals

- **Building or finishing Strata.** Strata is evaluated on its own track. Track QE designs `cetra-column` and `ScanSpec` so Strata fits cleanly, but ships on `ParquetSource` without waiting for Strata.
- **Replacing `sqlparser`.** We keep `sqlparser` (Apache-2.0, pure Rust, independent of DataFusion, using `GenericDialect`) as the SQL and filter DSL lexer and AST parser.
- **Full ANSI / PostgreSQL SQL compatibility.** Correlated subqueries in `SELECT`/`WHERE`, `WITH RECURSIVE`, `LATERAL` joins, `ROLLUP`/`CUBE`/`GROUPING SETS`, arbitrary non-equi joins without an equality key, and exotic `RANGE`/`GROUPS` window frames are explicitly out of scope and rejected with actionable error messages.
- **Changing unrelated HTTP, MCP, CLI or UI contracts.** All read endpoints, filter DSL syntax, cursor semantics, error codes (`invalid_filter`, `query_failed`, `query_timeout`, `query_too_large`, `busy`), JSON/CSV/NDJSON/Parquet export formats and `GET /schema` shapes remain preserved — **except** for the documented Shape B SQL exclusions, the unified `POST /sql` `columns[].type` vocabulary (D-QE-15, updated together with the web SQL console helpers, OpenAPI docs and fixtures in M7), and the expanded `function_catalog` in `GET /schema`.
- **Distributed multi-node execution.** Single-machine vertical scaling (D15) remains the target; internal clean pipeline boundaries keep future multi-node partitioning possible.
- **Tantivy full-text indexing.** Full-text search stays a scan over the time window (with `ScanSpec` substring pushdown ready for Strata's n-gram index); Tantivy remains post-MVP (D25).
- **Using `unsafe`.** `cetra-column` and `cetra-query` remain 100% safe Rust, following `AGENTS.md`.

---

## Architecture

### Crate layout and dependency graph

```text
                cetra-column          (leaf crate: DataType, Column, Batch, Selection,
                 ▲        ▲            sync MemoryPool / FairSpill, CancelToken,
                 │        │            checksummed binary batch framing; zero tokio/arrow/parquet)
                 │        │
            cetra-core    │           (IDs, enums, Span/Score ↔ Batch, schemas;
             ▲     ▲      │            no tokio, no arrow, no datafusion)
             │     │      │
       cetra-otel  │  cetra-query     (expr/, exec/, sql/, filter/, JSON/CSV/NDJSON encoders;
                   │   ▲   ▲           depends on cetra-column, cetra-core, sqlparser;
                   │   │   │           no datafusion, no arrow, no parquet, no object_store)
                   └───┼───┘
                       │
                 cetra-storage        (WAL, memtable, catalog, async admission wait, compaction,
                       ▲               deletion rewriter, media GC, verify/unwind, Parquet metadata
                       │               cache, ScanSource impls: ParquetSource / future StrataSource,
                       │               .parquet SQL export wrapper, TraceMerger, Scores LWW,
                       │               reads/, temporary Arrow↔Batch bridge during M1–M6)
                       │
                 cetra-server         (HTTP, MCP, SSE, SQLite views → BatchSource, API conversion;
                       ▲               no arrow, no datafusion, no parquet)
                       │
                  cetra-cli           (no arrow, no datafusion; dev-only Parquet test reader)
```

- `cetra-column` has **no workspace dependencies** and **no `tokio`, `arrow`, `datafusion` or `parquet` dependencies**, so `cetra-core`, `cetra-otel`, `cetra-query`, `cetra-storage`, `cetra-server`, and (later) **Strata** share the exact same zero-copy in-memory batch representation without pulling an async runtime into `cetra-core` or `cetra-otel`.
- `cetra-otel` still never depends on `cetra-storage` or `cetra-query` (preserving the parallel-build rule in `AGENTS.md`).
- `cetra-query` owns the pure in-memory expression compiler/evaluator, push-pipeline executor, Filter DSL compiler, Shape B SQL binder/planner/optimizer, and JSON/CSV/NDJSON encoders. It has **no `parquet`, `arrow`, `datafusion` or `object_store` dependencies**.
- `cetra-storage` owns all disk/S3 I/O, async admission waiting (`tokio::sync::Notify` around `cetra-column`'s synchronous `MemoryPool`), the WAL, memtable, catalog, compaction, deletion rewriter, media GC, `read_cache`, Parquet metadata cache, `ParquetSource` (and future `StrataSource`), the `.parquet` SQL export writer, `TraceMerger`, `Scores` LWW, and `reads/`.
- **Cargo `parquet` feature boundary:** root `Cargo.toml` declares `parquet` with `default-features = false` (no `arrow`, `async`, or `object_store`). While `ParquetSource` remains in `cetra-storage`, `cetra-storage` enables `parquet`'s storage features (`arrow`, `async`, `object_store`, `zstd`, `snap`, `lz4`, `flate2-zlib-rs`) — so Cargo still unifies those features inside `cetra-storage` until Strata lands — while keeping other crates (including `cetra-cli`'s dev-dep) off `parquet/arrow`, and allowing `cetra-storage` to drop `arrow`/`async`/`object_store` (retaining only `parquet/zstd` for `.parquet` SQL export) the moment Strata replaces `ParquetSource`.
- `cetra-server`'s SQL export test verifies `.parquet` output through a `cetra-storage` test helper rather than importing `datafusion::parquet` or adding a `parquet` dependency to `cetra-server`.
- During M1–M6, root `Cargo.toml` pins `sqlparser` to the exact major/minor version DataFusion 55.2 already pulls (`0.62`), avoiding duplicate versions under `cargo-deny`.

### Execution flow

```text
HTTP / MCP / CLI / Alerts / Online Rules / SSE / Storage Maintenance
  │
  ├── Structured reads, Filter DSL,    ──► Direct Pipeline Builder (no SQL planner)
  │   Compaction, Deletion, Media GC            │
  │                                             │
  └── SQL console / MCP / eval stats   ──► sqlparser (GenericDialect) ──► Sandbox Validator
                                                │
                                                ▼
                                         Binder & Optimizer
                                                │
                                                ▼
                                     Physical Pipeline Graph
                              (Bounded CPU Worker Pool + MemoryPool
                               + FairSpill + CancelToken + Spill Disk)
                                                │
           ┌────────────────────────────┬───────┴─────────────────────────┐
           ▼                            ▼                                 ▼
   Memtable Snapshot        File ScanSource (async I/O prefetch)    BatchSource
  (in-memory Batches)       ParquetSource now / StrataSource later  (server-owned
                           + read_cache + metadata_cache + unwind    experiment_items,
                           + tombstones + TraceMerger / Scores LWW   dataset_items)
```

---

### Contracts between parts

#### 1. `cetra-column` contract

**Type system.** Covers every stored column (spec 01) and every computed Shape B SQL / Filter DSL value:
- Scalars:
  - `Null` (untyped `NULL` literal),
  - `Bool`,
  - `I64` (all storage integers — including `spans.schema_version` (`u16`) and `traces.partial_count` (`i32`) — widen to `I64` in memory; on-disk encoders still pack them tightly),
  - `F64`,
  - `Decimal128 { precision, scale }` (default `(38, 18)` for deterministic cost sums in structured reads, D-A2-4; parameterized for SQL `CAST(x AS DECIMAL(p, s))`),
  - `TimestampNs { utc: bool }` (nanoseconds since epoch; `utc = true` for all stored timestamps and RFC 3339 `Z` outputs, `utc = false` for naive SQL timestamps such as `CAST(x AS TIMESTAMP)`),
  - `Date` (days since Unix epoch, for `CAST(x AS DATE)` and date expressions),
  - `Interval` (calendar-aware `months, days, nanos`, supporting `INTERVAL '1 month'`, `date_trunc` and `date_bin`),
  - `Utf8`,
  - `Binary` (variable-width bytes, e.g. `X'00ff'`, `encode`),
  - `FixedBytes8` (`SpanId`),
  - `FixedBytes16` (`TraceId`).
- Collections & structured columns:
  - `List(Box<DataType>)` (offsets + child `Column` — covers `List(Utf8)` for `tags`/`models`/`prompts`, `List(I64)`/`List(F64)` for `ARRAY_AGG` and array literals, and `List(List(Utf8))` for intermediate list aggregation),
  - `MapUtf8` (string-to-string maps for `metadata`, `attributes`, `resource_attributes`),
  - Dedicated typed `Events` and `Links` columns matching spec 01's fixed struct lists (D-QE-11), formatting to JSON only at API and SQL output boundaries.

**Column encodings.** Every column in a `Batch` is one of four representations:
- `Flat`: contiguous value buffers + optional null bitmap.
- `Const`: one scalar value (or null) repeated for the batch length (zero allocation for all-null columns, literals, partition constants and schema-evolution missing columns).
- `Dict`: dictionary of distinct values + compact integer codes (`u16` or `u32`) + optional null bitmap — preserves Parquet and Strata dictionary encoding into filtering and hashing without eager string expansion.
- `Rle`: run-length encoded values + run lengths — preserves sorted/low-cardinality runs from Strata.

Operators that benefit from encodings (`Filter`, `HashAgg`, `Facets`, `Project`) work on them directly; any operator requiring flat data calls a single `materialize` path tracked against `MemoryPool`.

**Selection vectors.** Every `Batch` carries a `Selection`:
- `All` (every row active),
- `Mask` (packed bitmap of passing rows), or
- `Indices` (compact `u32` list of active row indices).

Filtering updates `Selection` instead of copying 50 KiB `input`/`output` strings. Physical compaction (`take`) runs only at pipeline boundaries (e.g. before spilling, sorting or encoding results) or when selectivity drops below a threshold.

**Memory pool, admission control, fair spilling and cancellation.**
- `MemoryPool` (`cetra-column`, synchronous and `tokio`-free): replaces DataFusion's memory pool while preserving every spec 03 § Resource limits guarantee:
  - Process-wide global cap split into query budget and separate `compaction.max_memory` budget.
  - Synchronous `try_admit(initial_bytes)` + release-notification hook; `cetra-storage` wraps this asynchronously with `tokio::sync::Notify` to wait up to `query.admission_wait` before returning `503 busy` (D-A1-8).
  - `FairSpill` accounting (D-A1-13) dividing a query's cap fairly across concurrent spillable consumers while protecting unspillable allocations.
  - Every `Batch` buffer, hash table, sort run, accumulator and string-expression output reserves its byte footprint before allocating (`Batch::memory_bytes()` also drives memtable `flush.max_bytes` and `memtable.hard_limit`); failure returns `TooLarge` (`422 query_too_large`). Dropping any guard releases bytes immediately.
- `CancelToken`: cheap atomic flag shared across I/O prefetchers and CPU workers, checked on every batch boundary and inside long loops (regex, string scan, merge). Dropping a query or export stream triggers cancellation and synchronously drains in-flight CPU workers so no detached task holds `MemoryPool` reservations past drop.

**Checksummed binary batch framing.** Replaces Arrow IPC for WAL records and temporary spill files (+ LZ4 compression), preserving every hardening invariant of `wal/record.rs`:
- CRC-32C checksum over header + payload;
- Strict DoS/corruption bounds (`MAX_PAYLOAD = 256 MiB`, `MAX_DECODED_BYTES = 1 GiB`, LZ4 expansion ratio bomb check `<= 256 + 64`, schema and length validation on decode);
- Deterministic, zero-panic error reporting on corrupt or truncated frames (never panics on disk input).

#### 2. `ScanSource`, `ScanSpec`, and storage-internals contract

The boundary between `cetra-query` and any table/file implementation (`ParquetSource`, future `StrataSource`, memtable, or server-registered in-memory `BatchSource`).

A scan receives a `ScanSpec` describing:
- **Projection:** exact columns needed, plus **projected map keys** (e.g. `metadata['customer']`, `attributes['http.status_code']`). Strata reads a shredded sub-column; Parquet reads the map column.
- **Time bounds & pruning hints:**
  - nanosecond range on `start_time` (or `created_at` for scores), derived from the user window, `visible_from` (D-P-21), and `WHERE` predicates;
  - trace-anchor day widening (±1 day slack on file day pruning for `spans` and `traces`, exact on memtable, D86);
  - `QueryOptions::around` (±1 day pruning hint for trace lookups, D154).
- **ID lookups:** equality or `IN (...)` sets on binary IDs (`trace_id`, `span_id`) and string IDs (`session_id`, `user_id`), enabling bloom-filter and sort-key pruning.
- **Pushdown predicates:** conjunctive comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`, `IN`, `IS [NOT] NULL`, `@>`).
- **Substring search hints:** `column ILIKE '%needle%'` / `LIKE` predicates pushed down so Strata can use its n-gram frame index and unique-chunk scanning, while Parquet applies late materialization (filter light columns before reading heavy payload columns).
- **Sort & limit hints:** optional `limit` and requested ordering (e.g. `start_time DESC LIMIT 50`) for widening-window early stop.

**Mandatory pushdown safety invariants (enforced by storage sources, never violated by the optimizer):**
- **`TracesSource`:** only `start_time` (padded by `±trace_lookback`) and `trace_id` `=`/`IN` may be pushed below `TraceMerger` (spec 03 §6). All other filters evaluate *after* partials are merged.
- **`ScoresSource`:** only `created_at` may be pushed below LWW deduplication. Pushing `name`, `deleted`, or `value_num` below LWW would resurrect superseded versions.
- **Parquet schema evolution:** scanning an older file that lacks a newly added nullable column produces `Const(Null)` for that column (spec 03 §3).

**Row-level storage semantics owned by `cetra-storage`:**
- **Unified tombstone predicate (D-A1-3):** one `cetra-query` expression builder shared across reads, flush, compaction and deletion rewrites suppresses deleted traces/users/sessions/scores inline before rows reach operators.
- **`TracesSource`:** owns partial merging (`TraceMerger`, sorted merge by `trace_id` with spillable runs), widening-window list mode (`15m / 1h / 6h / 24h / whole-range`), and two-phase rank→fetch for `cost_total`, `duration_ns` and `total_tokens` sorts.
- **`ScoresSource`:** owns per-`score_id` last-writer-wins deduplication ordered by **`(updated_at DESC, ingest_time DESC)`** (spec 03 §5) and deleted-score suppression. *(Distinct from filter/read latest-score-per-target, which partitions by `(target_key, name)` and orders by `(updated_at DESC, score_id DESC)`, D97.)*
- **`span_list` vs `facets` deduplication:** `span_list` deduplicates adjacent span copies by `(start_time, span_id, trace_id)` keeping newest `ingest_time` (with the existing bounded refetch loop), whereas `facets` on `spans` intentionally skips deduplication (spec 05).
- **`visible_from` & time windows:** enforced inside every view definition (`spans`, `traces`, `scores`, `sessions`, and server-owned `experiment_items`) so no join, CTE or subquery can bypass them.

**I/O, caching, and fault isolation in `ParquetSource`:**
- **Metadata cache:** a bounded LRU cache of parsed Parquet footers and page indexes honoring `storage.query.metadata_cache_bytes` (default 512 MiB, spec 03 §4), replacing DataFusion's built-in metadata cache.
- **Async prefetch & `read_cache`:** Tokio tasks prefetch remote/disk byte ranges through `cetra-storage`'s `read_cache` (with range coalescing and catalog tail-length hints) and hand ready buffers to the CPU worker pool.
- **Corrupt-file guard & verification (`unwind` / `verify`):** every file decode runs inside a panic-catching guard that classifies decode/format panics and errors, records unreadable files into `Suspects` (spec 03 §10), and never crashes the server.

**Storage maintenance built on the same primitives (no DataFusion):**
- **Compaction merge & slicing:** streaming k-way merge of sorted files using `cetra-query`'s sort-merge operator under `compaction.max_memory` (preserving sorted output and derived `traces` recomputation, D88).
- **GDPR deletion candidate check & rewriter:** evaluates the unified tombstone predicate directly over `ScanSource` with `Limit(1)` early exit to skip unaffected files, and rewrites matching files streaming through `cetra-column` batches.
- **Media GC live-set scan:** streams projected `spans.media_refs` with tombstones directly via `ScanSource`.
- **Memtable:** stores `cetra-column::Batch`es, computes byte size from `Batch::memory_bytes()`, and caches derived trace partials as `cetra-column::Batch`es per `MemBatch`.

**In-memory `BatchSource` registration (for server-owned views):**
- `cetra-query` accepts caller-registered in-memory `BatchSource` tables. `cetra-server` builds `experiment_items` and `dataset_items` from `meta.db` (respecting the existing 1M-row / 256 MiB text budgets, D93, D-J-15) and registers them without `cetra-storage` ever touching `meta.db`.

#### 3. Push-pipeline execution contract

Queries compile to a DAG of **pipelines** separated by pipeline breakers (`HashAgg`, `Sort`, `HashJoin` build side, `Window` partition sort, `TraceMerger` barrier):

- **Bounded CPU worker pool:** one process-wide worker pool with an 8 MiB worker stack size (matching `cetra-cli`'s Tokio worker stack) runs synchronous batch loops at concurrency bounded by `storage.query.target_partitions`. Tokio handles only disk/S3/network I/O.
- **Batch flow & early stop:** operators process fixed-cap batches (exact default batch size tuned in M3; spec 05's `batch_size` note updates in M7 if changed from 8,192), return `Continue` or `Stop` (for `LIMIT` / `match_count` early exit), and check `CancelToken` every batch.
- **Thread-local aggregation & shared scans:**
  - `HashAgg`, `TopK`, `Facets`, and `WidgetAgg` keep lock-free thread-local state per worker and perform one final combine.
  - **`POST /facets` (with per-field error isolation matching spec 05):**
    - One shared Pass 1 evaluates the filter once and updates all valid requested `values`, `list values`, `map keys` and histogram summary statistics (`min`, `max`, `p50`, `p95`, `p99`) simultaneously. High-cardinality `values` facets (`user_id`, `session_id`, `metadata['k']`) use the spillable partitioned `HashAgg` path.
    - Exact 20+1 histogram bucket counts (spec 05) run at most one extra narrow Pass 2 over only the requested histogram numeric columns once `[min, p99)` is known — at most 2 scans total instead of up to 40.
    - **Partial-error & timeout contract:**
      - Malformed requests, invalid filters, or planning/context failures fail the whole request (`Err(...)`).
      - Invalid individual field specs (`bad field`, `limit`, `q`) fail only those fields before Pass 1; valid sibling fields still run.
      - If one field hits `TooLarge` during Pass 1, only that field records `Err(TooLarge)` and drops its accumulator immediately to free memory; sibling fields continue.
      - If Pass 1 hits the request deadline, every unfinished valid field receives `Err(Timeout)` while preserving pre-validated `Err(InvalidRequest)` slots.
      - If Pass 2 (histogram buckets) hits the deadline, Pass 1's completed `values`/`keys` fields still return `Ok(...)` and only unfinished histogram fields receive `Err(Timeout)`.
  - **`POST /dashboards/query`:**
    - Non-breakdown widgets compute all 1–6 measures in **one scan** per time range (plus a second scan of `[from − (to − from), from)` when `shape: total` sets `compare: true`, and the traces semi-join when a dashboard `filter` applies to `spans` or `scores`, spec 05).
    - Breakdown widgets (`limit` 1–20, `other: true`) run in **two bounded scans** per range: pass 1 ranks the top-`limit` groups by the first measure (spillable `HashAgg`); pass 2 computes all measures across buckets for those ≤20 groups plus one `other` group in one shared scan (avoiding `O(all_groups × 2,000 buckets)` accumulators).
- **Spilling (out-of-core execution):**
  - Streaming operators (`Scan`, `Filter`, `Project`, `TopK`, `Limit`, `Union`) run in small fixed RAM.
  - `Sort`, `HashAgg`, `HashJoin`, and `TraceMerger` spill runs/partitions to `<data_dir>/tmp/query/` using `cetra-column` binary framing, integrated with `Disk` to enforce `storage.min_free_bytes`, mapping `ENOSPC`/`EDQUOT` to `422 query_too_large`, and bounding open file descriptors during k-way merge (spec 03 § Resource needs).

#### 4. Filter DSL & Shape B SQL contract

**Parsing & sandboxing** (preserving D95, D96, D-A2-1 on `sqlparser` `GenericDialect`):
- Token/size/operator-count/recursion/table-count limits checked before planning (including `INTERSECT` and `EXCEPT` counting toward the 32-table cap).
- Filter DSL: identical grammar and behaviour to spec 05 § Filter DSL (`TRY_CAST` on the 5 types, `@>`, map indexing, `IS [NOT] DISTINCT FROM`, `BETWEEN`, `LIKE`/`ILIKE ... ESCAPE`, 9 allow-listed functions, `score()`/`score_label()`/`has_score()` compiled to hash left-joins on latest scores per `(target_key, name)` ordered by `(updated_at DESC, score_id DESC)`, max 8 score names).
- SQL console: one statement (`SELECT` / `WITH` / `UNION [ALL]` / `INTERSECT` / `EXCEPT` / `VALUES`, or plain `EXPLAIN` without `ANALYZE`/`VERBOSE`), unqualified references to the 6 views or in-scope CTEs, `{name:Type}` parameter binding, and an **explicit function allow-list** where every allocation reserves from `MemoryPool`.
- All 20 currently denied OOM/storage-internal functions (`range`, `generate_series`, `repeat`, `lpad`, `rpad`, `replace`, `regexp_replace`, `array_repeat`, `list_repeat`, `array_resize`, `list_resize`, `array_replace`, `array_replace_n`, `array_replace_all`, `list_replace`, `list_replace_n`, `list_replace_all`, `levenshtein`, `input_file_name`, `file_row_index`) continue returning explicit 400 errors naming the function.
- **`EXPLAIN` schema & GDPR redaction:** `EXPLAIN` returns the existing two-column schema `["plan_type", "plan"]` (`string`, `string`), redacting erased `user_id`/`session_id` tombstone lists as `<n redacted>` (spec 03 §7). `trace_list_plan` preserves `"mode=list"` / `"mode=range"` markers for `trace_bench`, and `trace_bench`'s missed-target diagnostic uses the engine's operator timing/row/byte profile instead of DataFusion's `EXPLAIN ANALYZE`.

**Supported Shape B SQL surface:**
- **Clauses:** `SELECT` (including `SELECT` without `FROM`, `VALUES`, expressions, aliases, `DISTINCT`, `CASE WHEN`), `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` (`ASC`/`DESC`, `NULLS FIRST`/`LAST`; PostgreSQL default `ASC → NULLS LAST`, `DESC → NULLS FIRST` in SQL, while structured list APIs stay `NULLS LAST` in both directions per spec 05), `LIMIT`, `OFFSET`.
- **Composition:** non-recursive `WITH` CTEs, derived tables in `FROM`, `UNION` / `UNION ALL`, `INTERSECT` / `EXCEPT` (lowered to hash semi/anti-joins, D-QE-14), `UNNEST(list_expr)` (preserving spec 05 § Sandbox item 1), and `IN (SELECT ...)` / `NOT IN (SELECT ...)` / `EXISTS` (planned as hash semi/anti-joins).
- **Joins:** `INNER`, `LEFT`, `RIGHT`, `FULL` equi-joins (plus residual non-equi filter on the join output), and bounded `CROSS JOIN` (strictly gated by `MemoryPool` and result byte caps).
- **Aggregates:** `COUNT`, `COUNT(DISTINCT ...)` (spillable), `SUM` (arbitrary user SQL `sum(f64)` returns `f64` unless cast; structured reads `sessions`, `user_list`, dashboards and alerts sum costs in `Decimal128(38, 18)` per D-A2-4), `AVG`, `MIN`, `MAX`, **`approx_percentile_cont(expr, p)`** (matching shipped UI examples, user docs and spec 07, backed by bounded-memory DDSketch/t-digest, D-QE-13), exact `percentile_cont(p) WITHIN GROUP (ORDER BY expr)` gated by `MemoryPool` (D-QE-13), `BOOL_OR`, `BOOL_AND`, `ARRAY_AGG` (gated by `MemoryPool`), and `FILTER (WHERE ...)`. The derived `sessions` view uses dedicated internal aggregates for first non-null `user_id` by `(start_time, trace_id)` and sorted distinct tag union (`tags`), keeping those out of arbitrary user SQL syntax.
- **Window functions:** `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, and running `SUM`/`COUNT`/`AVG`/`MIN`/`MAX` with `OVER ([PARTITION BY ...] [ORDER BY ...])` (default frame and `ROWS BETWEEN` bounds).
- **Curated scalar functions & operators (~50):**
  - String & binary: `lower`, `upper`, `length`/`char_length` (UTF-8 character count), `substr`/`substring`, `trim`/`ltrim`/`rtrim`, `left`, `right`, `split_part`, `concat`, `concat_ws`, `||` (string & array concatenation), `starts_with`, `ends_with`, `contains`, `strpos`/`position`, `like`/`ilike` (with `ESCAPE` and Unicode case-folding), `regexp_like`, `regexp_extract` (compiled once per expression with strict regex size cap → `400 query_failed`), `encode(binary, 'hex')`.
  - Time: `now`, `date_trunc`, `date_bin(stride, source [, origin])`, `extract`/`date_part`, `to_timestamp`, `INTERVAL` arithmetic, string-to-timestamp casts accepting RFC 3339 and `'YYYY-MM-DD HH:MM:SS'`.
  - Math & null handling: `abs`, `round`, `floor`, `ceil`, `sqrt`, `ln`, `log10`, `power`, `coalesce`, `nullif`, `greatest`, `least`, `isnan`, `isfinite`, `IS [NOT] DISTINCT FROM`, `BETWEEN`.
  - Array & Map: `array_has`, `array_has_all` (`@>`), `array_length`, `array_element` (`[i]`), `array_to_string`, `array_distinct`, `map_keys`, `map_values`, map index (`m['k']`).
  - JSON: `->` / `->>` (object key string or array index integer, PostgreSQL-style) and `json_extract` / `json_extract_text` (JSONPath `'$.a.b[0]'` via existing `serde_json_path`).
  - Type conversion: `CAST`, `TRY_CAST`, `::`.
- **Execution error semantics:** division by zero, checked integer overflow, invalid cast, regex too big, and non-finite/out-of-range trace cost in structured sums (spec 05) return `400 query_failed`. `NaN` sorts greater than `+inf` (matching IEEE total order / DataFusion) and serializes to `null` in JSON (D98).
- **Optimizer passes (small, deterministic rule set):**
  1. Constant folding & boolean simplification.
  2. Hex-literal ID rewrite (`trace_id = '<hex>'` → binary `FixedBytes16` comparison for bloom/sort-key pruning, D89).
  3. Projection pushdown (including extracting referenced `map['key']` accesses into `ScanSpec`).
  4. Filter pushdown respecting storage safety invariants (time bounds, ID lookups, narrow predicates, substring search hints).
  5. `ORDER BY ... LIMIT k` → `TopK(k)` rewrite (and widening-window trace-list pushdown).
  6. View expansion (`traces` hiding bookkeeping columns, `sessions` aggregate view, time window + `visible_from` injection).
- **Explicitly rejected (400 `invalid_filter` with clear reason and suggestion):** correlated subqueries, `WITH RECURSIVE`, `LATERAL`, `ROLLUP`/`CUBE`/`GROUPING SETS`, `GROUPS` / arbitrary `RANGE` window frames, DDL/DML, table functions, and all 20 denied OOM/internal functions.
- **`GET /schema` function catalog:** `cetra-query` exposes the curated Shape B function catalog with accurate `in_filter` / `in_sql` flags; `cetra-storage::function_catalog()` switches to it in M7 alongside updating `fixtures/api/static/schema.json` and running `just docs-gen`.

#### 5. Result & export contract

Preserves spec 05 § Sandbox item 8 (D98, D-H-8, D-A2-5):
- **JSON console results (`cetra-query`):** `{columns: [{name, type}], rows, truncated}`, max 10,000 rows, max 32 MiB encoded JSON, exact pre-row size estimation + pool reservation:
  - `TimestampNs { utc: true }` → RFC 3339 9-digit `Z`; `TimestampNs { utc: false }` → RFC 3339 9-digit without `Z`;
  - `Date` → ISO-8601 `YYYY-MM-DD`;
  - `Interval` → canonical SQL interval string;
  - `Decimal128` → decimal string (preserving exact precision for sorting/display);
  - `Binary` / `FixedBytes8` / `FixedBytes16` → lowercase hex string;
  - `F64` `NaN`/`±inf` → `null`;
  - `Events` → JSON array of `{time, name, attributes}` objects; `Links` → JSON array of `{trace_id, span_id, attributes}` objects with lowercase hex IDs;
  - `Null` → `null`.
- **`columns[].type` display names (D-QE-15):** unified with `GET /schema`'s vocabulary (`string`, `integer`, `float`, `decimal`, `boolean`, `timestamp`, `date`, `interval`, `binary`, `string_list`, `string_map`, `list`, `json`, `null`), with `Events`/`Links` rendered as `json` and raw `FixedBytes8`/`FixedBytes16` as `binary`. The web SQL console type parser (`web/src/lib/sql/types.ts`), charts, sorting, OpenAPI doc comments (`just openapi`) and API fixtures update to this vocabulary in M7.
- **CSV & NDJSON streaming exports (`cetra-query`):** up to 10M rows, 64 MiB chunk cap, CSV formula-injection quoting (`=`, `+`, `-`, `@`, tab, CR), using the same scalar/nested text formatting as JSON (empty CSV cell for `null`).
- **Parquet streaming exports (`cetra-storage` wrapper over `cetra-query` batch stream, D-QE-12):** preserves `.parquet` download from `POST /sql` with ZSTD level 1 compression and 32 MiB / 1M-row row groups gated by `MemoryPool`, using `parquet`'s low-level writer (`default-features = false` + `zstd`, no `arrow` feature). Exact Parquet physical/logical schema mapping:
  - `Bool` → `BOOLEAN`; `I64` → `INT64`; `F64` → `DOUBLE`; `Decimal128(p, s)` → `FIXED_LEN_BYTE_ARRAY(16)` (`DECIMAL(p, s)`); `TimestampNs { utc }` → `INT64` (`TIMESTAMP(NANOS, isAdjustedToUTC = utc)`); `Date` → `INT32` (`DATE`); `Utf8` → `BYTE_ARRAY` (`STRING`); `Binary` → `BYTE_ARRAY`; `FixedBytes8`/`FixedBytes16` → `FIXED_LEN_BYTE_ARRAY(8|16)`; `List(T)` → standard 3-level Parquet `LIST` (including nested `List(List(T))`); `MapUtf8` → standard Parquet `MAP<STRING, STRING>`; `Events`/`Links` → native Parquet `LIST` of `GROUP` matching spec 01's stored Parquet layout; `Interval` → `BYTE_ARRAY` (`STRING`, avoiding Parquet's lossy unsigned ms `INTERVAL`); `Null` → optional `BOOLEAN` column of all nulls.

#### 6. Differential oracle & cut-over strategy

During Milestones 1–6, DataFusion stays in `crates/storage` behind an internal test-only comparison harness (with the temporary `cetra-column` ↔ Arrow bridge also hosted inside `cetra-storage`, keeping `cetra-column` and `cetra-query` 100% Arrow-free) and `cetra-core`/`cetra-storage`'s **public** API signatures stay unchanged so every milestone ends with `just check` green:
- **Oracle comparison rules:**
  - Exact row-for-row and error-code parity on all deterministic filter, read and SQL queries.
  - Explicit relative-error tolerance (≤ 1% relative error) on sketch-based `approx_percentile_cont` / facet histogram `p50`–`p99` and resulting `[min, p99)` bucket counts (D-QE-13).
  - Explicit expected-diff rules for `columns[].type` (D-QE-15) and `EXPLAIN` plan text (verifying the two-column schema and GDPR `<n redacted>` redaction instead of identical DataFusion plan wording).
- **Benchmark fidelity:** M4 measures `trace_bench` directly on `cetra-column` batches (without Arrow bridge copy overhead in the timed path).
- **Atomic cut-over (M7):** switches `cetra-core`, `cetra-otel`, `cetra-storage`'s public APIs, `cetra-server`, `cetra-cli`, `web/` SQL helpers, OpenAPI (`just openapi`), API fixtures and generated docs (`just docs-gen`) onto `cetra-column` + `cetra-query`, deletes `datafusion` and `datafusion-datasource`, keeps `FORMAT_VERSION = 1` (since `cetra` is unreleased and `fixtures/storage/golden/RELEASES` is empty per spec 03 §12), and isolates Parquet/Arrow storage reads and writes inside `cetra-storage`'s file-format adapter ready for Strata.

---

## Decisions and trade-offs

| ID | Decision | Rejected alternative and why |
|---|---|---|
| D-QE-1 | **Replace DataFusion with our own pure-Rust query engine**, keeping `sqlparser` (`GenericDialect`) for parsing. | Keeping DataFusion: ~30+ heavy crates, planning overhead on small queries, untracked operator memory gaps, delayed cancellation drop, and poor impedance fit for Strata's compressed scans, shredded maps and n-gram search. Embedding DuckDB or `chDB`: C++ FFI violates the pure-Rust / no-`unsafe` rule, weakens multi-tenant SQL sandboxing (DB-wide memory limit), complicates static/cross builds, and cannot cleanly host `TraceMerger` or async S3 `read_cache`. |
| D-QE-2 | **Replace Arrow with a lean, `tokio`-free `cetra-column` crate** shared by `cetra-core`, `cetra-query`, `cetra-storage`, `cetra-server`, and (later) Strata. | Keeping Arrow `RecordBatch` inside the engine: models 50+ dynamic types (`Arc<dyn Array>` + downcasting), slows compilation, lacks native selection vectors, and forces Strata to decompress into flat Arrow arrays instead of passing `Dict`/`Rle`/`Const` blocks and selection masks directly. Pulling `tokio` into `cetra-column`: would unnecessarily infect `cetra-core` and `cetra-otel`. |
| D-QE-3 | **Decouple the engine from on-disk formats via `ScanSource` / `ScanSpec`.** Build and ship on `ParquetSource` (with our own bounded Parquet metadata cache) now; plug in `StrataSource` once Strata passes its workload evaluation. | Waiting for Strata to finish before starting QE: serializes two independent tracks unnecessarily. Hard-coding Parquet inside query operators: would force a second rewrite when Strata lands. |
| D-QE-4 | **Adopt curated analytical SQL ("Shape B")** for the SQL console and MCP, with an explicit function allow-list (including `approx_percentile_cont`, `UNNEST`, `encode`, and JSON extraction) and clear rejections for exotic ANSI corners. | Full ANSI/DataFusion SQL parity: 80% of a SQL engine's complexity lives in correlated subqueries, `LATERAL`, `ROLLUP`/`CUBE`, complex window frames and 150+ rarely used functions, with almost no real LLM-observability demand. Dropping SQL entirely (Filter DSL only): breaks D7, the SQL console UI, saved queries, and MCP `query_sql`. |
| D-QE-5 | **Use DuckDB-style synchronous push pipelines on a bounded CPU worker pool** (8 MiB worker stacks), with Tokio dedicated to disk/S3/network I/O. | Pull-based async `Stream`s (DataFusion model): adds async polling overhead per batch, risks blocking Tokio I/O workers on CPU-heavy scans, and makes shared-scan multi-consumer fan-out (`POST /facets`, multi-measure widgets) awkward. |
| D-QE-6 | **Spill to disk on buffering operators only** (`Sort`, `HashAgg`, `HashJoin`, `TraceMerger`) using `cetra-column` binary framing, enforcing `storage.min_free_bytes` via `Disk` and bounding open spill FDs. | In-memory only (fail with `422` at memory cap): breaks legitimate 30-day `GROUP BY user_id` / non-time sorted trace lists at the default 2 GiB cap. Spilling on every operator: unnecessary for streaming ops (`Scan`, `Filter`, `Project`, `TopK`). |
| D-QE-7 | **Use checksummed, bounded `cetra-column` binary framing (+ LZ4) for WAL records**, replacing Arrow IPC, while keeping `FORMAT_VERSION = 1` pre-release (spec 03 §12). | Keeping Arrow IPC in the WAL: would permanently pin `arrow-ipc` in `cetra-storage` and `cetra-core`. Bumping `FORMAT_VERSION` before v1: unnecessary while `fixtures/storage/golden/RELEASES` is empty. |
| D-QE-8 | **Keep `.parquet` export on `POST /sql`** in `cetra-storage` without pulling `parquet` or `arrow` into `cetra-column`, `cetra-query`, or `cetra-server`. | Dropping `.parquet` export: rejected by the user. Putting `parquet` inside `cetra-query`: would violate `cetra-query`'s zero-Parquet dependency boundary. |
| D-QE-9 | **Keep DataFusion and existing public Arrow/DataFusion signatures alongside the new engine during M1–M6** (differential test oracle + green `just check` on every milestone), then cut over atomically in M7. | Big-bang delete on day one: loses our strongest automated correctness oracle. Breaking `cetra-core`/`cetra-storage` public APIs in M6 before migrating `cetra-server`/`cetra-otel`: would leave M6 uncompilable. |
| D-QE-10 | **Dedicated `crates/column` (`cetra-column`) and `crates/query` (`cetra-query`) crates** rather than putting everything inside `crates/storage`. | Folding everything into `crates/storage`: would prevent Strata (and `cetra-core`) from sharing `Batch`/`Column` cleanly and keep `crates/storage` as one huge compile unit. |
| D-QE-11 | **Model `spans.events` and `spans.links` as dedicated typed `Events` and `Links` columns** in `cetra-column`. | Storing them as `Utf8` JSON strings: forces JSON serialization/parsing on ingest, flush and compaction; typed columns keep spec 01 lossless and format to JSON only at the API/SQL boundary. |
| D-QE-12 | **Implement `.parquet` SQL exports using the `parquet` crate's low-level writer** (`default-features = false` + `zstd` only, no `arrow` feature), with root `Cargo.toml` declaring `parquet` as `default-features = false`. | Writing a custom Parquet writer from scratch: unnecessary spec-interop risk with DuckDB/pandas/Polars when the low-level writer gives spec compliance and lets `cetra-storage` drop `arrow` cleanly once Strata replaces `ParquetSource`. |
| D-QE-13 | **Use bounded-memory sketches (DDSketch / t-digest) for dashboard widgets, facets and `approx_percentile_cont`, and exact `percentile_cont` in SQL gated by `MemoryPool`.** | Exact per-group sorting everywhere: 2,000 time buckets × 20 groups can blow memory on large windows; sketches keep dashboard and facet RAM O(1) per bucket while SQL retains exact percentiles when explicitly requested. |
| D-QE-14 | **Include `INTERSECT` and `EXCEPT` in Shape B SQL**, lowered to hash semi/anti-joins (keeping the existing 32-table limit from D-A2-1). | Supporting only `UNION` / `UNION ALL`: `INTERSECT` and `EXCEPT` are useful for comparing trace/user cohorts and add minimal complexity on top of `SemiJoin` / `AntiJoin`. |
| D-QE-15 | **Emit unified cetra `columns[].type` display names** (`string`, `integer`, `float`, `decimal`, `boolean`, `timestamp`, `date`, `interval`, `binary`, `string_list`, `string_map`, `list`, `json`, `null`) in `POST /sql` responses, aligned with `GET /schema`, updating spec 05, OpenAPI, the web SQL console helpers and fixtures together in M7. | Keeping DataFusion's Arrow debug format (`Utf8`, `Timestamp(Nanosecond, Some("UTC"))`): `cetra` is unreleased and Arrow's type printer leaks implementation details. Inventing a second `int64`/`float64` vocabulary separate from `GET /schema`: unnecessary inconsistency across the API and UI. |

---

## Open questions for the user

None left. Settled with the user: D-QE-11 to D-QE-15 (with D-QE-15's vocabulary unified with `GET /schema` and `web/src/lib/sql/types.ts`, OpenAPI and fixtures updated together in M7).

---

## Milestones

Milestones run in order. Each gets its own plan written just before it starts, from the code that exists then, and every milestone ends with `just check` green.

### M1 — `cetra-column` data plane and checksummed binary framing

**Delivers:**
- The `cetra-column` crate (zero `tokio`/Arrow/Parquet/workspace deps): full type system (`Null`, `Bool`, `I64`, `F64`, `Decimal128`, `TimestampNs`, `Date`, `Interval`, `Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`, `List`, `MapUtf8`, `Events`, `Links`), `Column` (`Flat`, `Const`, `Dict`, `Rle`), `Batch`, `Selection` (`All`, `Mask`, `Indices`), synchronous `MemoryPool` (global cap, query/compaction split, `try_admit`, `FairSpill`), and `CancelToken`.
- Checksummed, DoS-bounded binary batch framing (+ LZ4, CRC-32C, ratio/size limits) for WAL and spill files, with round-trip and fuzz tests.
- Temporary `cetra-column` ↔ Arrow `RecordBatch` conversion bridge hosted inside `cetra-storage` (never inside `cetra-column`), used by `ParquetSource` and differential tests during M2–M6.

**Depends on:** nothing.

**Leaves out:** expression evaluation, operators, SQL, and any change to `cetra-core` or `cetra-storage` public APIs.

### M2 — Vectorized expressions (`expr/`) and Filter DSL parity

**Delivers:**
- The `cetra-query` crate's expression IR, binder, and vectorized batch evaluator over `cetra-column` (preserving `Selection` vectors, reserving every allocation from `MemoryPool`, enforcing division-by-zero / overflow / regex-size / non-finite-cost rules).
- The curated ~50 scalar functions & operators (string/binary incl. `encode`, `LIKE`/`ILIKE ... ESCAPE`, `regexp_*`, time/calendar `Interval`/`date_trunc`/`date_bin`, math/nulls, `IS [NOT] DISTINCT FROM`, `BETWEEN`, `||`, array, map, JSON `->`/`->>`/`json_extract*`, `CAST`/`TRY_CAST`).
- The Filter DSL compiler (validation, `TRY_CAST`, map access, `@>`, score-function metadata) and the unified tombstone predicate builder (D-A1-3), verified against DataFusion across the full existing `filter/` test suite, plus fuzz and OOM tests.

**Depends on:** M1.

**Leaves out:** pipeline execution, aggregates, joins, storage scans, and SQL statement planning.

### M3 — Push-pipeline executor (`exec/`) and `ScanSource`

**Delivers:**
- The DuckDB-style push-pipeline scheduler on a bounded process-wide CPU worker pool (8 MiB stacks), with cooperative `CancelToken` checks, synchronous worker drain on drop, and `Continue`/`Stop` early termination.
- Core operators: `Scan`, `Filter`, `Project`, `TopK`, `Limit`, `Union`, `Unnest`, `Window` (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, running aggregates), and spillable `Sort`, `HashAgg` (`Decimal128` cost sums, `approx_percentile_cont`, exact `percentile_cont`, `COUNT(DISTINCT)`, `ARRAY_AGG`, `FILTER (WHERE ...)`, dedicated `sessions` aggregates), and `HashJoin` / `SemiJoin` / `AntiJoin`, integrated with `Disk` (`storage.min_free_bytes`) and bounded spill FDs.
- Async `admission_wait` wrapper (`tokio::sync::Notify` → `503 busy`) in `cetra-storage` around `cetra-column`'s `MemoryPool`.
- The `ScanSource` / `ScanSpec` contract, generic in-memory `BatchSource` registration interface, and `cetra-storage` implementations:
  - memtable snapshot scan;
  - `ParquetSource` with bounded LRU Parquet metadata cache (`storage.query.metadata_cache_bytes`), async I/O prefetch through `read_cache`, bloom/page/anchor/`around` pruning, late materialization, schema-evolution `Const(Null)` handling, and corrupt-file panic guard (`unwind`/`verify` → `Suspects`);
  - inline tombstone suppression;
  - native spillable `TraceMerger` (sorted merge, widening-window list mode, two-phase rank→fetch, respecting pushdown safety bounds);
  - native `ScoresSource` (`(updated_at DESC, ingest_time DESC)` LWW dedup, respecting pushdown safety bounds).
- Spill-under-tight-memory tests, admission-control (`503 busy`) tests, sub-millisecond cancellation/drain tests, and storage model-test verification.

**Depends on:** M1, M2.

**Leaves out:** porting `cetra-storage::reads`, storage maintenance (compaction/deletion/media GC), SQL statement planning, and `cetra-server` SQLite views.

### M4 — Product reads (`reads/`) and performance verification

**Delivers:**
- Every structured read and validation entry point implemented on `cetra-query` + `ScanSource`:
  - `trace_list` (including two-phase rank→fetch for `cost_total`, `duration_ns` and `total_tokens`), `trace_list_plan` (preserving `"mode=list"`/`"mode=range"` markers), `trace_detail`, `trace_spans`, `error_spans`, `trace_refresh`, `span_list` (preserving adjacent-copy dedup + refetch loop), `span_detail`, `span_refresh`, `match_count`;
  - `session_list`, `session_detail`, `session_extras`, `user_list`, `user_detail`, `user_extras`;
  - `score_list`, `latest_scores`, `score_name_counts`, `scores_by_id`;
  - shared-scan multi-field `POST /facets` (with spillable high-cardinality values, exact histogram buckets, and per-field error/timeout isolation);
  - shared-scan `POST /dashboards/query` widgets (one-pass non-breakdown, two-pass bounded breakdown, `compare: true` previous-period scan, traces semi-join for `spans`/`scores`);
  - `alert_value`, annotation-queue `targets` (`trace_targets`, `span_targets`, `session_targets`, `filter_*_targets`), deletion scope reads, `prompt_metrics`, `experiment_*`, and `validate_filter`/`validate_shape`/`validate_expressions`.
- Differential verification against DataFusion (exact parity on deterministic reads; ≤ 1% relative tolerance on sketch percentiles) across the full `reads/` suite.
- Direct `cetra-column` `trace_bench` comparison (no Arrow bridge copy in timed paths, using the engine's operator profile for missed-target diagnostics) proving the new engine matches or beats DataFusion on latency and peak RSS.

**Depends on:** M3.

**Leaves out:** the SQL console (`POST /sql`), `sql_check`, MCP `query_sql`, storage maintenance, public API cut-over, and deleting DataFusion.

### M5 — Shape B SQL frontend (`sql/`) and exports

**Delivers:**
- The Shape B SQL frontend in `cetra-query` over `sqlparser` (`GenericDialect`): sandbox validator (preserving all 20 denied functions and 32-table cap), `{name:Type}` parameter binding, binder/typechecker over the 6 views (with time-window and `visible_from` injection), logical plan, rule-based optimizer (constant folding, hex-ID rewrite, projection & map-key pushdown, safety-bounded filter & substring pushdown into `ScanSpec`, `TopK` pushdown), two-column GDPR-redacted `EXPLAIN`, `sql_check`, and push-pipeline compilation.
- Direct `cetra-column` encoders in `cetra-query` for JSON console responses (32 MiB cap, 10k row cap, exact size estimation, D-QE-15 `columns[].type` names, exact `Decimal128`/`Interval`/`Date`/`Events`/`Links` serialization), CSV (with formula-injection quoting) and NDJSON, plus streaming `.parquet` export in `cetra-storage` using `parquet`'s low-level writer (ZSTD level 1, 32 MiB / 1M-row row groups, exact Parquet schema mapping, no `arrow` feature, D-QE-12).
- The new Shape B function catalog built and unit-tested inside `cetra-query` (without yet switching `cetra-storage::function_catalog()`, so `schema.json` and `just check-docs` stay green until M7).
- Full differential and sandbox-security verification against DataFusion on the Shape B SQL suite (including shipped UI examples, `eval/stats` SQL queries, `UNNEST`, `INTERSECT`/`EXCEPT`, `approx_percentile_cont`, `EXPLAIN` schema & redaction, MCP `query_sql`), plus a new `sql` fuzz target added to `fuzz/Cargo.toml` and `justfile`.

**Depends on:** M3, M4.

**Leaves out:** storage maintenance migration, switching `cetra-storage::function_catalog()`, removing DataFusion, and removing Arrow from `cetra-core`/`cetra-server`.

### M6 — Storage internals, WAL framing and memtable migration

**Delivers:**
- Internal migration of all remaining `cetra-storage` subsystems off DataFusion and onto `cetra-column` / `cetra-query` (keeping `cetra-core` and `cetra-storage`'s public Arrow/DataFusion signatures bridged at the crate boundary until M7 so the workspace compiles and passes `just check` cleanly):
  - compaction k-way merge and slicing under `compaction.max_memory`;
  - GDPR deletion candidate checking (`Limit(1)`) and file rewriting using the unified tombstone predicate;
  - media GC live-set scan;
  - `anchor`, `dedup`, `partials`, `flush`, `verify`/`unwind`, `golden_tests/dump`, and `fuzz_entry`;
  - WAL binary batch framing (+ LZ4, CRC-32C, DoS/bomb bounds, keeping `FORMAT_VERSION = 1` pre-release, regenerating WAL fuzz seeds via `just fuzz-seed`);
  - `memtable` storing `cetra-column::Batch`es with `Batch::memory_bytes()` and cached trace-partial `Batch`es, verified against spec 03 § Resource needs.
- Isolation of all Parquet storage reading/writing (and its temporary Arrow conversion) inside one internal storage file-format adapter module.

**Depends on:** M4, M5.

**Leaves out:** breaking public API changes in `cetra-core`/`cetra-storage`, migrating `cetra-otel`/`cetra-server`/`cetra-cli`/`web/`, deleting DataFusion from `Cargo.toml`, and final docs/spec cut-over.

### M7 — Atomic workspace cut-over, DataFusion removal, and full verification

**Delivers:**
- Atomic switch of all cross-crate contracts off Arrow and DataFusion (removing the temporary M1–M6 Arrow bridge):
  - `cetra-core` (`Span`/`Score` ↔ `cetra-column::Batch`, schemas; zero `arrow` dep);
  - `cetra-otel` (`fuzz_entry` updated to `cetra-core`'s `Batch` API);
  - `cetra-storage` public APIs (`Storage::append_*`, `reads::*`, `ProjectQuery`, `QueryContext`, `RunError`, `QueryError`, `MediaError`, `CompactionError`, `function_catalog()`) and all `cetra-storage` benches/integration tests (`ingest_bench`, `trace_bench`, `s3_chaos`, `it/*`);
  - `cetra-server` production and test callers (ingest, scores, read APIs, `eval/trace_load`, `eval/stats`, `issues/errors`, `deletion`, `http/error`, `datasets/sql_views` registering `BatchSource` directly, test Parquet verification via `cetra-storage` helper; zero `arrow`, `datafusion` or `parquet` deps);
  - `cetra-cli` (migrating its Parquet test reader off `parquet/arrow`; zero `arrow` or `datafusion` deps);
  - `web/src/lib/sql/types.ts`, SQL console chart/sort/grid tests, OpenAPI doc comments (`just openapi`), and API fixtures (`schema.json`, `sql-result.json`, assistant conversation fixture) updated for D-QE-15 and the Shape B function catalog.
- Removal of `datafusion` and `datafusion-datasource` from root `Cargo.toml`, `Cargo.lock` and `deny.toml` (retaining `arrow`/`parquet`/`object_store` pins and advisory entries in `deny.toml` until Strata replaces `ParquetSource`), with root `Cargo.toml` declaring `parquet` as `default-features = false`.
- Spec, decision, roadmap and docs updates: `docs/spec/README.md`, `01-data-model.md`, `03-storage.md`, `05-query.md`, `07-alerts.md`, `09-api.md`, `12-operations.md`, `mvp-decisions.md`, `roadmap.md`, `AGENTS.md`, `website/` (`sql-console.mdx`, `configuration-reference.md`, regenerated `sql-schema.md`), and `skills/cetra/references/` (`just docs-gen`).
- Full green verification: `just check`, `just e2e`, `trace_bench`, `ingest_bench`, `just resources`, `just resources-memory`, `just stress`, `just lazyfs`, `just soak`, crash/model/fuzz tests, and recorded compile-time and binary-size measurements.

**Depends on:** M6.

**Leaves out:** switching storage files from Parquet to Strata (a separate follow-up once Strata clears its workload evaluation).
