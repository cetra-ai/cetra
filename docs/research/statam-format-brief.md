# Statam (`crates/statam`) — Session Brief, Constraints & Design Charter

**Date:** 2026-11-26  
**Status:** Charter for a dedicated new session (supersedes the general-purpose Strata v0 prototype in `docs/research/file-format/prototype-plan.md`).  
**Target branch / worktree:** Create a **new git worktree branched from `qe-query-engine`** (e.g. `git worktree add -b statam .claude/worktrees/statam qe-query-engine`). **Do NOT edit `main` or `.claude/worktrees/qe-query-engine`** (Track QE is actively running there).  
**Target machine:** Run research, implementation, and all benchmarks on **`precision-5770-vm`**.  
**Crate path:** `crates/statam` (`cetra-statam`).

---

## 1. Mission

Design, specify, prototype, and prove a **100% Cetra-native storage file format (`statam`)** to replace Parquet as Cetra's on-disk and S3 table format.

**Non-negotiable bar:** Statam must beat Parquet (compared honestly against **all three** baselines: `pq_default`, `pq_tuned`, and `pq_cetra`) across **every** axis that matters to Cetra:
1. **Read latency** — filtered narrow scans, time-bucketed aggregates, top-k lists, point lookups (`trace_detail`, `span_detail`), `map['key']` lookups, and substring search (`%needle%`).
2. **Write throughput & CPU/memory cost** — both latency-critical L0 flushes and high-density L1 compactions.
3. **S3 network efficiency** — fewer HTTP GETs, lower RTT amplification, fewer bytes transferred, and clean alignment with Cetra's **1 MiB on-disk `ReadCache`**.
4. **Storage footprint** — smaller total bytes on disk/S3 across real agent trajectories, multi-turn chat sessions, narrow spans/scores, and no catastrophic regression on adversarial/incompressible data.
5. **Lifecycle & maintenance speed** — fast L1 compaction merge, O(1) day-level retention deletion, and fast, airtight GDPR erasure rewrites.

---

## 2. Why a New Format Now (Post-Track QE Context)

The earlier Strata research (`docs/research/file-format/`) assumed Parquet/Vortex had to plug into **Apache Arrow + Apache DataFusion 55**. That constraint is now gone:
- Track QE (`qe-query-engine` branch) replaces Arrow and DataFusion with **`cetra-column`** (`crates/column`) and **`cetra-query`** (`crates/query`).
- **Statam is 100% Cetra-native.** It does **not** need Arrow compatibility, DataFusion adapters, or general-purpose lakehouse interop (`.parquet` SQL exports remain a separate thin low-level writer in `cetra-storage`).
- Statam speaks **`cetra-column`** natively:
  - Exact `DataType` set: `Null`, `Bool`, `I64`, `F64`, `Decimal128 { precision, scale }`, `TimestampNs { utc }`, `Date`, `Interval`, `Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`, `List(Box<DataType>)`, `MapUtf8`, `Events`, `Links`.
  - Native compressed vector representations: `Column::{Flat, Const, Dict, Rle}` and `Selection::{All, Mask, Indices}`. Statam can return `Dict`, `Rle`, and `Const` columns directly into `cetra-query` without decompressing to flat buffers!
  - Decoupled engine boundary: plugs directly into `cetra-query::exec::ScanSource` / `ScanSpec` (`projection`, `filters`, `time_range`, `id_lookups`, `substring_hints`, `map_keys`, `limit`, `around_ns`) and `cetra-storage`'s isolated `file_format` adapter.

### Existing Code & Research to Read First
Do **not** reinvent or duplicate work already completed in this repo. Read these first:
- `docs/plans/query-engine/design.md` — `cetra-column`, `cetra-query`, `ScanSource`/`ScanSpec`, and `file_format` adapter architecture.
- `crates/column/src/` — `types.rs`, `column.rs`, `batch.rs`, `selection.rs`, `memory.rs`, `frame.rs`.
- `crates/query/src/exec/source.rs` — `ScanSource`, `ScanSpec`, `IdLookup`, `SubstringHint`, `PartitionStream`.
- `crates/storage/src/read_cache.rs` — Cetra's 1 MiB S3 block cache (`BLOCK = 1 << 20`, CRC-32C per block, LRU eviction).
- `crates/storage/src/parquet.rs` & `crates/storage/src/engine/parquet_source.rs` — exact current Parquet layout, bloom filters, page index, sorting columns, metadata cache, and corrupt-file quarantine contract (`Suspects`).
- `docs/spec/01-data-model.md`, `03-storage.md`, `05-query.md` — exact schemas, retention, compaction, and GDPR deletion semantics.
- `docs/research/file-format/` — prior research already done on Parquet (`parquet.md`), Vortex (`vortex.md`), media/compression/VCS/HTTP-range formats (`other-formats-a.md`), LSM/columnar/search/TSDB formats (`other-formats-b.md`), the payload-layer microbenchmark (`README.md`), and Strata v0 (`prototype-plan.md`).

---

## 3. Settled Architectural Decisions & Hard Constraints

All of the following decisions are **already settled with the user** — treat them as hard inputs to the design:

### 3.1 Crate Path & `unsafe` / SIMD Policy
- Crate: **`crates/statam`** (`cetra-statam`).
- **Controlled `unsafe` is explicitly allowed** in `crates/statam` (opting out of the workspace-wide `unsafe_code = "forbid"` lint for this crate only), under strict guardrails:
  1. `unsafe` is permitted **only** for **SIMD kernels** (AVX2 / AVX-512 / NEON with a safe portable scalar fallback) and **tight inner encode/decode loops** (e.g., bit-unpacking, SIMD trigram/substring scanning, SIMD dictionary/RLE/ALP/FSST loops, proven-aligned buffer views).
  2. **NEVER omit bounds, length, UTF-8, or invariant checks on untrusted bytes** read from disk or S3. Corrupt or malicious files MUST always return a clean `Err` (feeding `Suspects` / `VerifyQueue`) — never UB, out-of-bounds reads, uncatchable OOM aborts, or panics.
  3. Every `unsafe` block MUST carry a `// SAFETY:` comment stating the exact invariant.
  4. **Mandatory verification:** every `unsafe` path (via its scalar/portable path) MUST pass **`cargo miri test`**, and `crates/statam` MUST ship a committed **`cargo fuzz`** target (`statam_file`) exercising untrusted bytes against the decoder, metadata parser, bloom/skip indexes, and chunk-pack reader.

### 3.2 Asymmetric Write Profiles (`L0Fast` vs `L1Compact`)
Cetra's write path has two fundamentally different jobs; Statam MUST expose explicit write profiles rather than one compromise setting:
- **`WriteProfile::L0Fast` (memtable flushes):**
  - Self-contained immutable file (zero cross-file chunk dependencies — simple, fast, crash-safe flush).
  - Optimized for low flush latency and low CPU/RAM overhead: fast lightweight encodings (bitpacking, delta/FOR, dictionary, RLE, LZ4 or ZSTD-1), fast bloom filters + zone maps; avoid expensive dictionary training or heavy multi-pass indexing that would stall ingestion.
- **`WriteProfile::L1Compact` (background compactions):**
  - Optimized for maximum compression ratio, minimum S3 bytes/GETs, and fastest query speed.
  - Full encoding cascades (e.g., ALP/Patas for floats, FSST / ZSTD-3 / trained dicts for strings, FastLanes-style SIMD bitpacking), shredded hot `metadata`/`attributes`/`resource_attributes` map keys, full trigram/n-gram substring skip indexes, and **shared per-`(project, day)` chunk packs**.

### 3.3 Per-`(project, day)` Shared Immutable Chunk Store (L1 Only)
- **L0 files** remain 100% self-contained immutable files.
- **L1 compactions** may write large deduplicated payloads (e.g., `input`, `output`, `system_instructions`, `events`, large attribute values) into **immutable content-addressed chunk packs scoped strictly to `(project_id, day)`**.
- Why per-`(project, day)` is the sweet spot:
  1. **Cross-file dedup within a day:** captures repeated system prompts, tool schemas, RAG documents, and growing multi-turn agent/chat histories across traces and sessions in the same day — not just within one file.
  2. **O(1) retention deletion:** dropping an expired day removes the entire `(project, day)` prefix (data files + chunk packs) with **zero reference counting or cross-day GC**.
  3. **GDPR erasure invariant (CRITICAL):** When GDPR deletion rewrites files in a `(project, day)` to erase a user, session, or trace, **erased payload bytes MUST actually be purged from the day's chunk packs** — they must NEVER remain readable in an orphaned/shared pack on disk or S3! Design the chunk-pack layout and compaction/rewrite protocol so GDPR rewrites can either:
     - copy surviving chunks byte-for-byte (without recompressing!) into a new immutable pack and retire the old pack atomically via `catalog.db`, or
     - partition/reference packs cleanly so dead erased bytes are deterministically deleted on rewrite completion.

### 3.4 Storage Medium: Local Disk + S3 + 1 MiB `ReadCache` Alignment
- Every S3 read passes through `crates/storage/src/read_cache.rs`, which caches immutable files in **1 MiB (`BLOCK = 1 << 20`) aligned blocks** with CRC-32C.
- Design Statam's physical layout explicitly around **1 MiB cache blocks and S3 RTT minimization**:
  - **1-GET tail read:** pack the zero-parse footer, column directory, zone maps, sort-key index, and contiguous bloom section into the tail so opening + pruning a cold file takes **1 S3 GET** (≤ 1 MiB whenever possible, cached in a single `ReadCache` block).
  - **1–2 GET point lookup (`trace_detail` / `span_detail`):** co-locate or tightly pack narrow columns and small/inline payloads (or chunk-pack slices) so fetching a single trace's spans reads minimal blocks and **never** pulls multi-megabyte pruned segments (avoiding Vortex's 66–80 MB point-lookup pitfall).
  - **Block-aligned / cache-friendly boundaries:** evaluate aligning major sections or chunk-pack frames to avoid straddling unnecessary 1 MiB `ReadCache` block boundaries.

### 3.5 Encryption-at-Rest Readiness (Optional Envelope Slot — Option B)
- Reserve a clean format-level header/footer flag and metadata envelope slot for **optional per-file and per-chunk-pack AEAD encryption** (e.g. XChaCha20-Poly1305 or AES-256-GCM with per-file DEK wrapped by a tenant/project KEK, per-block/frame nonces + auth tags supporting authenticated random access without decrypting whole files).
- Stage 1–2 runs unencrypted by default (**zero CPU/size overhead** when the encryption flag is off), while guaranteeing that enabling encryption later requires **zero breaking format change**.

### 3.6 Exact Cetra Table Profiles & Access Patterns
Statam serves Cetra's 3 exact tables (`docs/spec/01-data-model.md`):
1. **`spans` (57 columns), sorted by `(trace_id ASC, start_time ASC)`:**
   - Mix of narrow high-selectivity columns (`start_time`, `end_time`, `duration_ns`, `span_kind`, `status`, `model`, `provider`, `environment`, `release`, `cost_*`, `*_tokens`, `user_id`, `session_id`, `prompt_*`), fixed-width IDs (`trace_id` 16B, `span_id` 8B, `parent_span_id` 8B?), lists (`tags`, `media_refs`), wide dynamic maps (`attributes`, `resource_attributes`, `metadata`), structured lists (`events`, `links`), and massive repetitive text payloads (`input`, `output`, `system_instructions`, `input_preview`, `output_preview`).
   - Queries: `trace_detail`/`span_detail` point lookups by `trace_id`/`span_id`; `span_list`; `facets` & `widgets` (narrow scans + `map['key']` + `tags`); substring search (`input`, `output`, `name`, `system_instructions`); media GC (`[media_refs]` only); GDPR deletion `holds_covered` (`Limit(1)` on key columns).
2. **`traces` (32 columns, rolled-up partials), sorted by `(start_time ASC, trace_id ASC)`:**
   - Mostly narrow columns + `tags`, `models`, `prompts`, `metadata`, `input_preview`, `output_preview`.
   - Queries: `trace_list` (`mode=list` widening-window `start_time DESC` and two-phase rank→fetch on `cost_total`/`duration_ns`/`total_tokens`), `sessions` & `users` rollups, `facets`, `widgets`, `alerts`.
3. **`scores` (24 columns), sorted by `(created_at ASC)`:**
   - Narrow columns + `score_id`, `trace_id`, `span_id`, `session_id`, `name`, `source`, `data_type`, `value_num`, `value_str`, `comment`, `metadata`.
   - Queries: `score_list`, `latest_scores` (by `trace_id`/`session_id`), `score_name_counts`, `eval/stats` SQL, `experiment_scores`.

### 3.7 Security, Robustness & Privacy
- Never panic or OOM-abort on malformed files (enforce strict size/count/ratio limits before allocating, matching `cetra-column::frame` and `cetra-storage` fuzzing standards).
- Per-block/section checksums (CRC-32C / xxHash3 / BLAKE3 — evaluate speed & hardware acceleration) so silent bitrot and torn writes are detected cleanly.
- Privacy-safe `Debug` and error messages (never log payload text, user/session IDs, or map values).

---

## 4. Mandatory Phase 1: Cross-Domain Format Analysis

Before designing Statam's binary layout, the session MUST conduct a focused **cross-domain format study** (saved in `docs/research/statam/prior-art.md`), building **on top of** what already exists in `docs/research/file-format/{parquet,vortex,other-formats-a,other-formats-b}.md` (do not repeat basics already covered there; go deeper on gaps and Cetra-specific mechanisms):

1. **Video Game Asset & Streaming Containers** *(currently missing from `other-formats-{a,b}.md`!)*:
   - **Unreal Engine 5 IoStore (`.utoc` / `.ucas`)**: compact flat table-of-contents, fixed-size compression blocks (e.g. 64 KiB), content-addressed chunk IDs, deduplicated bulk data, zero-parse metadata, streaming locality.
   - **EA Frostbite CAS / DirectStorage / PlayStation Kraken streaming layouts**: aligning compressed blocks to storage/cache transfer units (directly analogous to Cetra's 1 MiB S3 `ReadCache`!), I/O coalescing without over-reading pruned blocks.
   - **Valve Source 2 VPK**: directory tree + inline preload bytes for tiny assets + external archive packs (directly analogous to inline small previews/payloads vs external chunk packs!).
   - **Trained compression dictionaries (Zstd dict / Oodle)** for thousands of small repetitive structured blobs (OTel JSON `events`, `attributes`, short `input`/`output` turns).
2. **Video & Media Containers** *(deepen beyond `other-formats-a.md`)*:
   - **ISOBMFF / MP4 (`moov`/`mdat`/`sidx` fast-start, fragmented MP4)** & **Matroska (`Cues`)**: single-fetch seek indexes, sync-point spacing for O(1) random access over HTTP Range requests.
3. **Compression, Dedup & Read-Only Filesystems**:
   - **Zstd seekable format**, **Zstd `--long`**, **trained dictionaries**, **SquashFS / EROFS** (fragment tail-packing for small values, block alignment), **FastCDC / Gear-hash**, **Git packfiles**, and **LLM-domain structural chunking**:
     - **Message-boundary / JSON-array structural splitting** for OTel GenAI `input` arrays (where turn `k+1` is literally turn `k`'s message array + 2 new messages — structural message-level chunking can achieve near-100% dedup at far lower CPU cost than byte-level rolling-hash CDC!).
     - Prefix/shared-dictionary dedup for system prompts and tool schemas.
4. **Modern Columnar & Analytical Formats**:
   - **Parquet** (what it gets right and its structural bottlenecks: Thrift footer parse, per-page isolated compression, 1-column-per-leaf map/struct explosion, no text index).
   - **Vortex** (cascading encodings, compute-on-compressed — and why it failed on point lookups: 8192-row variable-width segments + over-aggressive I/O coalescing).
   - **Lance v2** (mini-block vs full-zip layouts for narrow vs wide columns, O(1) random access without scanning row groups).
   - **Meta Nimble** (flat-map / shredded-map encodings with selective key streams — directly relevant to `attributes`, `resource_attributes`, `metadata`!).
   - **FastLanes** (SIMD-transposed 1024-value vectors for ultra-fast AVX2/NEON bit-unpacking and delta decoding).
   - **DuckDB & ClickHouse** (ALP/Chimp floats, FSST random-access string compression, Roaring/trigram skip indexes, `LowCardinality`, dynamic JSON subcolumns).

**Required deliverable of Phase 1:** A concrete **"Steal vs Reject" synthesis table** mapping every candidate technique to Cetra's exact tables (`spans`, `traces`, `scores`), write profiles (`L0Fast` vs `L1Compact`), and 4 workloads (`W1`–`W4`).

---

## 5. Three-Stage Gated Execution Roadmap (Option B)

The session MUST follow a strict **3-stage gated progression** — do NOT jump into `cetra-storage` integration before Statam proves itself on the adversarial benchmark suite!

### Stage 1 — Cross-Domain Analysis & Binary Specification (`docs/research/statam/`)
Deliverables:
- `docs/research/statam/prior-art.md` — cross-domain format analysis + "Steal vs Reject" synthesis matrix.
- `docs/research/statam/spec-v0.md` — complete, unambiguous binary specification for Statam v0:
  - File header, zero-parse footer, section directory, alignment rules (1 MiB `ReadCache` awareness), optional AEAD encryption envelope slot, checksum scheme.
  - Narrow-column encoding suite & how decoded vectors map directly to `cetra-column::Column::{Flat, Const, Dict, Rle}` + `Selection`.
  - Shredded map layout (`MapUtf8`) & typed `Events`/`Links` layout.
  - Wide-payload layer: inline small-value threshold, structural message-level vs CDC chunking, L0 self-contained mode vs L1 per-`(project, day)` immutable chunk packs, GDPR purge/rewrite protocol.
  - Pruning & search indexes: sort-key index, zone maps, contiguous bloom section, trigram/n-gram substring skip index.
  - Exact `L0Fast` vs `L1Compact` profile definitions.
  - Threat model & corrupt-file / OOM bounds table.

### Stage 2 — Standalone Prototype & Adversarial 4-Workload Benchmark (`crates/statam`)
Deliverables:
- `crates/statam` crate implementing:
  - `L0Fast` and `L1Compact` writers (`cetra_column::Batch` -> Statam files + L1 per-`(project, day)` chunk packs).
  - Async/sync reader implementing pruning, late materialization, `Dict`/`Rle`/`Const` pass-through, `map['key']` selective reads, substring skip index, and chunk-pack resolution.
  - Targeted SIMD kernels (`unsafe` guarded with `// SAFETY:`, portable scalar fallback, `cargo miri test` passing).
  - GDPR rewrite helper (proving erased bytes are physically absent from rewritten packs).
  - Fuzz target (`fuzz/fuzz_targets/statam_file.rs`) + corpus + replay tests.
- **Adversarial 4-Workload Benchmark Suite** (run on **`precision-5770-vm`**, comparing Statam `L0Fast` & `L1Compact` head-to-head against `pq_default`, `pq_tuned`, and `pq_cetra` on identical data, identical queries, local warm/cold AND simulated S3 with realistic latency e.g. 25 ms/GET + 100 MB/s + 1 MiB `ReadCache` model):
  - **W1 — Real Agent Trajectories:** public SWE-agent / coding-agent trajectories (massive conversation-prefix & system-prompt repetition, large tool outputs).
  - **W2 — Multi-Turn Chat Sessions:** cross-trace session history repetition across a day, moderate payloads.
  - **W3 — High-Throughput Narrow Spans & Scores:** millions of narrow rows (minimal/null payloads), heavy filtered scans, `facets`, `widgets` time-bucketed rollups, `trace_list` top-k, point lookups.
  - **W4 — Adversarial / Incompressible / High-Cardinality:** random high-entropy prompts, high-cardinality UUIDs, wide dynamic maps with thousands of distinct keys, tiny 1-row L0 flushes, mixed nulls, `NaN`/`±inf`, empty lists/maps.

#### Hard Go/No-Go Gate (End of Stage 2)
Statam may proceed to Stage 3 **only if ALL of the following hold** on `precision-5770-vm`:
1. **Size:** Strictly smaller than ALL Parquet baselines (`pq_default`, `pq_tuned`, `pq_cetra`) on **W1, W2, W3**, and within ≤ 5% (no regression) on **W4** (incompressible).
2. **Write speed:** `L0Fast` strictly faster than `pq_cetra` on all 4 workloads; `L1Compact` competitive with or faster than `pq_cetra` while delivering superior density.
3. **Read latency (local & simulated S3):** Strictly faster than ALL Parquet baselines across narrow scans, filtered aggregates, top-k, `map['key']`, point lookups (`trace_detail`/`span_detail`), and substring search (`%needle%`) on **W1–W3**, with zero catastrophic regressions on **W4**.
4. **S3 efficiency:** Cold file open + prune in **≤ 1 GET** (≤ 1 MiB tail); point lookup in **≤ 2 GETs** and strictly fewer S3 bytes than `pq_tuned` and `pq_cetra`.
5. **Safety & correctness:** Bit-exact round-trip across all 15 `cetra-column::DataType`s + edge cases (`NaN`, nulls, empty lists/maps), `cargo miri test` clean, `statam_file` fuzzing clean (zero panics/OOMs), GDPR erasure test verifying 0 bytes of erased payloads remain in disk/pack files.

### Stage 3 — `cetra-storage` Integration (Only After Stage 2 Gate Passes!)
Deliverables:
- Implement `StatamSource` (`ScanSource`) and `StatamFileWriter` behind `cetra-storage`'s `file_format` adapter, plus catalog/retention/GDPR chunk-pack lifecycle integration in `catalog.db`, `compaction/`, `deletion.rs`, and `verify.rs`.
- Keep `parquet` ONLY for `POST /sql` `.parquet` streaming export (`parquet_export.rs`), eliminating `arrow` and Parquet storage reads from `cetra-storage` completely!
- Full `trace_bench`, `ingest_bench`, crash/model/lazyfs/soak/fuzz/e2e verification, and spec/decision updates (`docs/spec/03-storage.md`, `docs/mvp-decisions.md`).

---

## 6. Quick-Start Checklist for the New Session

1. Verify you are on **`precision-5770-vm`** (record CPU model, SIMD flags `lscpu`, RAM, NVMe in your benchmark report).
2. Create a fresh worktree branched from `qe-query-engine`:
   ```bash
   git worktree add -b statam .claude/worktrees/statam qe-query-engine
   ```
3. Read `docs/plans/query-engine/design.md`, `crates/column/src/`, `crates/query/src/exec/source.rs`, `crates/storage/src/read_cache.rs`, and `docs/research/file-format/README.md`.
4. Execute **Stage 1** (`docs/research/statam/prior-art.md` + `spec-v0.md`) and get user sign-off on the spec before writing `crates/statam`.
5. Execute **Stage 2** (`crates/statam` + 4-workload adversarial benchmark + Miri + fuzz) and present the full benchmark report against the Go/No-Go gate before touching `cetra-storage` (Stage 3).
