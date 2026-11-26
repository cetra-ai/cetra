# Statam v0 (`crates/statam`) — Binary Specification & Design Document

**Date:** 2026-11-26  
**Status:** Stage 1 deliverable (Binary Specification + Architecture & Milestone Design Doc)  
**Crate:** `crates/statam` (`cetra-statam`)  
**Supersedes:** `docs/research/file-format/prototype-plan.md` (Strata v0)

---

## 1. Goals, Non-Goals & Settled Constraints

### Goals
1. **100% Cetra-native storage format** speaking `cetra-column` (`DataType`, `Schema`, `Batch`, `Column::{Flat, Const, Dict, Rle}`, `Selection::{All, Mask, Indices}`, `Bitmap`) directly with zero Arrow/DataFusion dependency in the format core.
2. **Beat all three Parquet baselines (`pq_default`, `pq_tuned`, `pq_cetra`) on every Cetra axis:**
   - Strictly smaller on `W1` (agent trajectories), `W2` (multi-turn chat), `W3` (narrow spans/scores), and within ≤ 5% on `W4` (adversarial/incompressible).
   - `WriteProfile::L0Fast` strictly faster than `pq_cetra` on all 4 workloads; `WriteProfile::L1Compact` competitive with or faster than `pq_cetra` at higher compression density.
   - Strictly lower read latency (local warm/cold and simulated S3 with 25 ms/GET + 100 MB/s + 1 MiB `ReadCache`) on narrow scans, aggregates, top-k, `map['key']`, point lookups (`trace_detail`/`span_detail`), and substring search (`LIKE`/`ILIKE`).
   - Cold open + prune in **≤ 1 GET** (≤ 1 MiB tail); point lookup in **≤ 2 GETs** and strictly fewer S3 bytes than `pq_tuned` and `pq_cetra`.
   - Fast L1 compaction merge (zero recompression of surviving chunks), O(1) day retention drop, and **airtight GDPR erasure** (0 bytes of erased payloads surviving on disk or S3).
3. **Controlled, audited `unsafe` SIMD** inside `crates/statam` only (AVX2 / NEON with safe portable scalar fallback), verified by `cargo miri test` and a committed `cargo fuzz` target (`statam_file`), with zero panics, zero OOM aborts, and privacy-safe `Debug`/errors.
4. **Encryption-at-rest readiness:** reserved header/footer envelope slot for optional per-file / per-pack AEAD encryption (zero byte/CPU overhead when off; non-breaking to enable later).

### Non-Goals
- General-purpose lakehouse/Arrow/DataFusion interop (`.parquet` SQL exports remain a thin low-level writer in `cetra-storage`).
- Cross-day chunk deduplication (packs are strictly scoped to `(project_id, day)`).
- Full-text inverted indexing / Tantivy (adaptive per-frame trigram skip index + unique-chunk scanning covers Cetra's `%needle%` queries).
- Modifying `cetra-storage`, `cetra-core`, `cetra-column`, or `cetra-query` before Stage 2 passes all 5 Go/No-Go gates.

---

## 2. High-Level Architecture

Statam defines **two immutable binary file types** backed by an abstract byte-range reader (`StatamReader` / `RangeReader` trait supporting local files, in-memory buffers, and simulated/real S3 + 1 MiB `ReadCache`):

1. **Data File (`.statam`):** Stores one sorted table slice (`spans`, `traces`, `scores`, or generic `cetra_column::Schema`) in **PAX Row Groups (16,384 rows)** made of **Mini-Blocks (4,096 rows)**, with a **Zero-Parse Hot Tail (target ≤ 1 MiB)** containing schema, fixed-width column directory, block zone maps, sort-key index, hot `trace_id` bloom filter, and small file-level dictionaries.
   - Under `WriteProfile::L0Fast`: **100% self-contained** (no external `.stpk` pack). Fast single-pass encodings (`Const`, `Rle`, FastLanes `ForBitpack`/`DeltaBitpack`, bounded `Dict`, `Lz4`, AVX2 SBBF bloom).
   - Under `WriteProfile::L1Compact`: Full encoding cascade (`Const`, `Rle`, sorted `Dict`, FastLanes `DeltaBitpack`/`ForBitpack`, `Alp`, `Fsst`, `ShreddedMap`, `Zstd(3)`), adaptive trigram skip indexes, and **shared per-`(project_id, day)` Chunk Packs (`.stpk`)** for wide payload columns.
2. **Chunk Pack (`.stpk`, L1 only):** Immutable content-addressed payload archive scoped strictly to one `(project_id, day)`, keyed by **BLAKE3-128** (`[u8; 16]`) chunk hashes. Stores deduplicated chunks in independently compressed 64 KiB raw / ~16–32 KiB compressed frames (1 MiB `ReadCache` boundary-aware), plus a zero-parse tail TOC (`FrameTable`, `ChunkTable`, `TrigramSkipSection`, `EdgeTable`, optional `DictSection`).

```text
  cetra_column::Batch ──► StatamWriter (L0Fast | L1Compact)
                                │
                 ┌──────────────┴──────────────┐
                 ▼                             ▼
           <id>.statam                   <pack_id>.stpk  (L1Compact only,
   ┌───────────────────────────┐         scoped to (project_id, day))
   │ Header (32 B)             │         ┌───────────────────────────┐
   │ RowGroup 0..G-1           │         │ PackHeader (32 B)         │
   │  ├─ Core Narrow Sub-RG    │         │ Frames 0..F-1 (64 KiB raw,│
   │  ├─ Wide-Row Sub-RG       │───────► │   1 MiB boundary guarded) │
   │  └─ L0 Payload Frames     │ chunk   │ Trigram Skip + Edge Table │
   │ Lazy Index Sections       │  refs   │ Optional DictSection      │
   │ Optional AEAD Envelope    │         │ Pack TOC (Frame + Chunk)  │
   │ ═════ HOT TAIL ≤ 1 MiB ══ │         │ PackPostscript (64 B)     │
   │  Schema + PackRefs        │         └───────────────────────────┘
   │  Column Directory (48 B)  │
   │  RG & MiniBlock Zone Maps │
   │  Hot Bloom (trace_id)     │
   │  Small File-Level Dicts   │
   │  Postscript (64 B)        │
   └───────────────────────────┘
```

### Public Crate API Surface (`crates/statam`)
- **Config & Profiles:** `WriteProfile::{L0Fast, L1Compact}`, `WriteOptions`, `TableKind::{Generic, Spans, Traces, Scores}`, `EncryptionSlotConfig`.
- **Writers:**
  - `StatamWriter::new(schema, options, pack_writer: Option<&mut ChunkPackBuilder>)` -> `write_batch(&Batch)` -> `finish(writer) -> Result<FileStats, StatamError>`
  - `ChunkPackBuilder` (accumulates deduplicated chunks across one or more L1 files in a `(project_id, day)`, writes immutable `.stpk`, returns `ChunkPackMeta`).
- **Readers & I/O Abstraction:**
  - `RangeReader` trait (`file_len(&self) -> u64`, `read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError>`, `read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError>`) + built-in `SliceReader`, `FileReader`, `SimulatedS3Reader` (with 1 MiB `ReadCache`, configurable RTT/bandwidth, and exact `IoCounters { gets, bytes_transferred, cache_hits, cache_misses, waves }`).
  - `StatamFileMeta` (parsed from 1-GET Hot Tail, cacheable in a metadata cache, exposes `schema`, `row_count`, `hot_tail_len`, `pack_refs`, `prune_file(&ScanPredicate) -> bool`).
  - `StatamReader::open(reader, pack_resolver)` / `StatamReader::open_with_meta(reader, meta, pack_resolver)` -> `scan(&ScanRequest) -> Result<Vec<Batch>, StatamError>` (plus streaming `StatamBatchIter`).
  - `ScanRequest`: mirrors `cetra_query::exec::ScanSpec` (`projection`, `map_keys`, `time_range`, `id_lookups`, `predicates`, `substrings`, `limit`, `coalesce_mode: CoalesceMode::{Auto, Point, Scan}`).
- **Lifecycle & GDPR Helpers:**
  - `gdpr_rewrite_day(live_files, pack_store, erased_row_predicate) -> Result<GdprRewriteResult, StatamError>`: rewrites affected `.statam` files, rebuilds `.stpk` packs copying 100%-live frames byte-for-byte, decompressing & purging mixed frames, dropping dead frames and old trained dicts, and deleting retired packs.
- **Error Type:** `StatamError` (`thiserror`), privacy-safe `Display`/`Debug` (never includes payload text, user/session IDs, or map values), cleanly classifies `Corrupt { reason }`, `UnsupportedVersion`, `UnsupportedFeature`, `BoundsExceeded`, `ChecksumMismatch`, `EncryptionRequired`, `SchemaMismatch`, `Io`, `Column(ColumnError)`.

---

## 3. Binary Format Specification: `.statam` Data File

All integers are **little-endian**. Every byte offset is an absolute `u64` from byte 0 of the file unless noted as tail-relative. Every section and block is protected by **CRC-32C** (`crc32c`).

### 3.1 File Header (32 bytes, offset 0)

| Offset | Size | Field | Description |
|---|---|---|---|
| 0 | 4 | `magic` | `b"STM0"` (`0x53 0x54 0x4D 0x30`) |
| 4 | 2 | `format_version` | `1` (`u16`) |
| 6 | 2 | `flags` | Bitflags (`u16`): `0x0001` `ENCRYPTED`, `0x0002` `L1_COMPACT`, `0x0004` `HAS_PACK_REFS`, `0x0008` `HAS_SHREDDED_MAPS`, `0x0010` `HAS_TRIGRAM_INDEX`, `0x0020` `COMPACT_INLINE_FILE` |
| 8 | 1 | `table_kind` | `0 = Generic`, `1 = Spans`, `2 = Traces`, `3 = Scores` |
| 9 | 1 | `checksum_alg` | `1 = CRC32C` |
| 10 | 2 | `required_features` | Required feature mask (`0` in v1; reader MUST reject unknown nonzero bits) |
| 12 | 8 | `enc_envelope_offset` | `0` if `!ENCRYPTED`; else absolute offset of `EncryptionEnvelope` |
| 20 | 4 | `enc_envelope_len` | `0` if `!ENCRYPTED`; else byte length of `EncryptionEnvelope` |
| 24 | 4 | `reserved` | `0` |
| 28 | 4 | `header_crc32c` | `crc32c(bytes[0..28])` |

### 3.2 Optional AEAD Encryption Envelope Slot (Reserved for Stage 3+)

When `flags & FLAG_ENCRYPTED == 0`: **0 bytes** written, `enc_envelope_offset == 0`, `enc_envelope_len == 0`.  
When `flags & FLAG_ENCRYPTED != 0`, `enc_envelope_offset..enc_envelope_offset + enc_envelope_len` holds:
- `envelope_version: u16` (`1`)
- `cipher_suite: u16` (`1 = AES-256-GCM`, `2 = XChaCha20-Poly1305`)
- `key_id_len: u16`, `wrapped_dek_len: u16`, `nonce_prefix: [u8; 12]`, `aad_binding: [u8; 32]` (BLAKE3 of unencrypted header + file UUID), `key_id: [u8]`, `wrapped_dek: [u8]`, `envelope_crc32c: u32`.
- Per-block/frame nonces derive deterministically from `nonce_prefix || block_or_frame_id` with a 16-byte AEAD authentication tag appended to each encrypted payload, supporting authenticated random access without decrypting whole files.
- A reader without a decryption key encountering `FLAG_ENCRYPTED` immediately returns `Err(StatamError::EncryptionRequired)`.

### 3.3 Postscript (64 bytes, final 64 bytes of file)

A cold reader fetches `min(file_len, tail_hint.unwrap_or(1 MiB))` from the end of the file in **1 GET**, reads the last 64 bytes, validates `magic` and `postscript_crc32c`, and if `hot_tail_len <= fetched.len()`, has the **entire Hot Tail in memory with zero additional I/O**.

| Offset | Size | Field | Description |
|---|---|---|---|
| 0 | 4 | `hot_tail_len` | Total bytes of Hot Tail **including** this 64 B Postscript |
| 4 | 2 | `format_version` | Must match header (`1`) |
| 6 | 2 | `flags` | Must match header `flags` |
| 8 | 8 | `row_count` | Total logical rows in file |
| 16 | 4 | `num_columns` | Logical column count (`<= 1024`) |
| 20 | 4 | `num_row_groups` | Number of PAX Row Groups (`G`) |
| 24 | 4 | `schema_off` | Offset of Schema section relative to Hot Tail start |
| 28 | 4 | `col_dir_off` | Offset of Column Directory relative to Hot Tail start |
| 32 | 4 | `block_index_off` | Offset of Block/Zone-Map Index relative to Hot Tail start |
| 36 | 4 | `hot_bloom_off` | Offset of Hot Bloom section relative to Hot Tail start (`0` if absent) |
| 40 | 4 | `hot_dict_off` | Offset of Hot Dict section relative to Hot Tail start (`0` if absent) |
| 44 | 4 | `lazy_index_dir_off` | Offset of Lazy Index Directory relative to Hot Tail start (`0` if absent) |
| 48 | 4 | `hot_tail_crc32c` | `crc32c` over Hot Tail bytes `[0 .. hot_tail_len - 64]` |
| 52 | 4 | `required_features` | Must match header `required_features` |
| 56 | 4 | `postscript_crc32c` | `crc32c(postscript[0..56])` |
| 60 | 4 | `magic` | `b"0MTS"` (`0x30 0x4D 0x54 0x53`) |

**Hot Tail Size Target:** Kept `<= 1 MiB` (typical 32–400 KiB). If a massive file's `hot_tail_len` would exceed `960 KiB`, large dictionaries (> 8 KiB) and secondary blooms (`session_id`, `user_id`, `score_id`) spill into Lazy Index Sections while keeping Postscript + Schema + Column Directory + Row-Group Zone Maps + `trace_id` bloom strictly inside the ≤ 1 MiB Hot Tail.

### 3.4 Hot Tail Sections

1. **Schema & Table Metadata (`schema_off`):**
   - `table_kind: u8`, `sort_col_count: u8`, `sort_cols: [u16; sort_col_count]`
   - `pack_ref_count: u16`, `pack_ids: [[u8; 16]; pack_ref_count]` (UUIDs of referenced `.stpk` packs)
   - `field_count: u16`, followed by each field: `name_len: u16`, `name_utf8`, `type_tag: u8` (exact 1:1 mapping to all 16 `cetra_column::DataType` variants + `Decimal128(p,s)` / `TimestampNs(utc)` / recursive `List(inner)` up to depth 8), `nullable: u8`, `locality_group: u8` (`0 = Core`, `1 = WideRow`, `2 = ExternalPayload`).
   - Optional `ShreddedMapDir`: for each shredded `MapUtf8` column, lists `col_idx: u16`, `shredded_key_count: u16`, and for each promoted key: `key_utf8`, `sub_col_idx: u16` (index into the extended physical column directory!), `residual_count: u32` (`0` = key never appears in residual map → skip residual on `map['key']`!).

2. **Column Directory (`col_dir_off`):** Fixed-width **48 bytes** per physical column (logical columns `0..num_columns` followed by shredded map sub-columns `num_columns..num_phys_columns`):
   - `phys_col_id: u16`, `logical_col_id: u16`, `locality_group: u8`, `col_flags: u8` (bit 0: `HAS_FILE_DICT`, bit 1: `DICT_SORTED`, bit 2: `IS_PAYLOAD_REFS`, bit 3: `IS_SHREDDED_SUBCOL`, bit 4: `HAS_INLINE_PAYLOAD_FALLBACK`), `reserved: u16`
   - `num_blocks: u32` (number of 4,096-row mini-blocks for this column)
   - `first_block_idx: u32` (index into flat `BlockEntry` array — O(1) lookup!)
   - `dict_offset: u64` (tail-relative if bit 5 `DICT_IN_HOT_TAIL`, else absolute file offset), `dict_len: u32`, `dict_entry_count: u32`
   - `bloom_offset: u64`, `bloom_len: u32`, `null_count_total: u32`

3. **Row-Group & Mini-Block Directory (`block_index_off`):**
   - `RowGroupEntry` (`G` entries, **40 bytes** each):
     - `first_row: u32`, `row_count: u32` (≤ 16,384)
     - `core_offset: u64`, `core_len: u32` (contiguous byte span of all Core columns in this RG!)
     - `wide_offset: u64`, `wide_len: u32` (contiguous byte span of all WideRow columns in this RG!)
     - `l0_payload_offset: u64`, `l0_payload_len: u32` (contiguous L0 payload span for this RG, `0` in L1)
   - `BlockEntry` (`sum(num_blocks)` entries, **64 bytes** each — aligned, zero-parse):
     - `first_row: u32`, `row_count: u32` (≤ 4,096)
     - `offset: u64` (absolute file offset; `0` for `Const`/`ConstNull` inline blocks!)
     - `stored_len: u32`, `raw_len: u32`
     - `encoding: u8` (see §4), `codec: u8` (`0=None, 1=Lz4, 2=Zstd`), `block_flags: u8` (bit 0: `HAS_NULLS`, bit 1: `ALL_NULL`, bit 2: `MIN_MAX_VALID`, bit 3: `HAS_NAN`), `aux: u8` (e.g. bit_width or ALP exponent)
     - `null_count: u32`
     - `min_inline: [u8; 16]` (exact 8 B LE for `I64`/`TimestampNs`/`F64`, 4 B LE for `Date`, 16 B for `FixedBytes16` `trace_id`, 8 B for `FixedBytes8`, 4 B LE code for `Dict`, or 16 B UTF-8 prefix for `Utf8`)
     - `max_inline: [u8; 16]` (same; UTF-8 max prefix incremented at last byte if truncated so range pruning is always sound!)
   - **Sort-Key Index:** Because every `BlockEntry` for the primary sort column (`trace_id` on `spans`, `start_time` on `traces`, `created_at` on `scores`) stores exact 16 B / 8 B `min_inline` and `max_inline` contiguously in the Hot Tail, binary searching the sort key is **O(log B)** directly on `BlockEntry` with **zero extra index bytes or I/O**!

4. **Hot Bloom Section (`hot_bloom_off`):**
   - Stores **Split-Block Bloom Filters (SBBF)** (256-bit = 8 × `u32` blocks, 8-lane SIMD probe using the 8 odd multiplicative constants `SALT = [0x47b6137b, 0x44974d91, 0x8824ad5b, 0xa2b7289d, 0x705495c7, 0x2df1424b, 0x9efc4947, 0x5c6bfb31]`, hashed with BLAKE3-64 or fast Murmur/SplitMix64 key hash).
   - For `spans` and `traces`, `trace_id` bloom filter (FPP = 0.01, ~10 bits/distinct key) sits in the **Hot Tail** so cold `trace_id` misses prune in **1 GET**. Secondary blooms (`session_id`, `user_id`, `score_id`) live in the Hot Tail if total tail remains ≤ 768 KiB, otherwise in a **contiguous Lazy Bloom Section** (1 GET per file for all secondary blooms, never 1-per-RG like Parquet).

### 3.5 Physical Block Framing (Data Region)

Every non-inline block stored in the data region has a **16-byte header**:
- `magic: [u8; 2]` (`b"SB"`), `encoding: u8`, `codec: u8`
- `raw_len: u32`, `stored_len: u32`, `crc32c: u32` (computed over the `stored_len` payload bytes immediately following the header).
- **Incompressibility guard:** Before emitting a compressed block, the writer checks `compressed.len() + 16 < raw.len()`. If false, it writes `codec = Codec::None` with the raw bytes verbatim.

---

## 4. Narrow & Structured Column Encoding Suite

Every mini-block (≤ 4,096 rows) picks its encoding via lightweight sampling and maps directly to `cetra-column::Column`:

| Code | Encoding | Applicable `DataType`s | Output `cetra_column::Column` | Mechanism |
|---|---|---|---|---|
| `0` | `ConstNull` | All | `Column::Const(Null)` | **0 bytes** in data region. |
| `1` | `Const` | Scalars (≤ 16 B inline) or file-dict code | `Column::Const(v)` | Value stored inline in `BlockEntry::min_inline` (**0 bytes** in data region!). |
| `2` | `Rle` | All scalars, `FixedBytes8/16`, dict codes | `Column::Rle` (or `Const`) | Run values + FastLanes-bitpacked `run_ends: Vec<u32>`. Chosen when `runs <= rows / 4`. |
| `3` | `ForBitpack` | `I64`, `TimestampNs`, `Date`, `Decimal128` (low 64), dict codes | `Column::Flat` (or `Dict` codes) | `base: i64` + `bit_width: u8` + FastLanes-1024 SIMD interleaved packed deltas `(v - base)`. |
| `4` | `DeltaBitpack` | `TimestampNs`, `I64`, `Date` (monotonic/sorted) | `Column::Flat` | FastLanes-1024 `04261537` parallel SIMD delta + bitpack (or zigzag delta for non-monotonic clustered values). |
| `5` | `Alp` | `F64` (`cost_*`, `value_num`) | `Column::Flat(F64)` | SIGMOD 2024 ALP: `(exp: u8, fac: u8)` decimal scaling + FastLanes FOR-bitpacked `i64` + sparse exception list `(row_idx: u16, exact_bits: u64)` guaranteeing 100% IEEE-754 bit-exact round-trip (`NaN`, `±inf`, `-0.0`). |
| `6` | `Dict` | `Utf8`, `FixedBytes8/16`, `Binary`, `List(Utf8)` leaf | `Column::Dict` directly! | File-level dictionary (sorted in `L1Compact`, insertion-order bounded ≤ 4096 in `L0Fast`) + FastLanes FOR/RLE-encoded `u32` codes + optional null bitmap. |
| `7` | `Fsst` | `Utf8`, `Binary` (medium/high cardinality) | `Column::Flat(Utf8/Binary)` | Static ≤ 255-entry symbol table (1–8 B symbols, `0xFF` escape) + FastLanes-packed offsets + compressed symbol bytes; supports O(1) per-row decode. |
| `8` | `BitpackBool` | `Bool` | `Column::Flat(Bool)` | Packed LE `u64` bitmap bytes (zero-copy into `cetra_column::Bitmap::from_bytes`). |
| `9` | `Plain` | All fixed/var types (fallback / `W4`) | `Column::Flat` | Contiguous raw fixed bytes or `(offsets: u32, data_bytes)` with optional outer `Lz4`/`Zstd` (subject to incompressibility guard). |
| `10` | `ListEncoded` | `List(T)` | `Column::Flat(List)` | FastLanes FOR-bitpacked lengths/offsets + recursively encoded child column (e.g., `Dict` for `tags`/`models`/`prompts`/`conventions`!). |
| `11` | `ShreddedMap` | `MapUtf8` (`L1Compact`) | `Column::Flat(MapUtf8)` (or single sub-col for `map_keys`!) | Hot keys (≥ 5% presence, ≤ 64 keys) stored as physical sub-columns with `in_map` presence bitmap + `value_null` bitmap + typed encoding; rare/null/dup keys in compact key-dict residual map. |
| `12` | `EventsStruct` | `Events` | `Column::Flat(Events)` | Decomposed sub-streams: FastLanes list lengths + Delta-bitpacked `times: i64` + Dict-encoded `names: Utf8` + FSST/LZ4/Zstd `attributes: Option<Utf8>`. |
| `13` | `LinksStruct` | `Links` | `Column::Flat(Links)` | Decomposed sub-streams: FastLanes list lengths + raw `trace_ids: [u8;16]` + raw `span_ids: [u8;8]` + FSST/LZ4/Zstd `attributes: Option<Utf8>`. |
| `14` | `PayloadRefs` | Heavy `Utf8` (`input`, `output`, `system_instructions`, `tool_definitions`) | `Column::Flat(Utf8)` | Hybrid inline (≤ 256 B stored directly in block) + tail-sharing chunk-ref pages (`back 1..=32`, `keep_prefix`, delta-varint `(pack_idx, chunk_id)`, keyframe depth ≤ 15) resolving against `.stpk` (L1) or L0 payload frames. |

**Null Bitmap Representation:** Inside any nullable block (`HAS_NULLS` flag set, not `ALL_NULL`), a compact validity bitmap (`row_count.div_ceil(8)` bytes, `1 = valid, 0 = null`) precedes the encoded values. Non-null blocks omit the bitmap completely (**0 bytes**).

---

## 5. Wide-Payload Layer & Per-`(project, day)` Chunk Packs (`.stpk`)

### 5.1 Chunking Algorithm (`L1Compact`)
1. Values `<= INLINE_PAYLOAD_THRESHOLD` (**256 B**): stored inline in the `PayloadRefs` block (tag bit `0 = inline`). Zero pack lookup!
2. Values `> 256 B`:
   - If the value starts with `[` (after optional ASCII whitespace): run the **Structural JSON Array Chunker** (zero-alloc state machine tracking top-level bracket depth, quotes, escapes):
     - Cut between top-level array elements once accumulated chunk size `>= MIN_CHUNK` (`256 B`).
     - Keep a trailing `]` separate if preceding bytes `>= 256 B` so growing turn `k -> k+1` reuses 100% of turn `k`'s final message chunk!
     - Any single JSON element `> MAX_CHUNK` (`8,192 B`) is sub-chunked via **Gear-hash FastCDC** (min `512 B`, avg `2,048 B`, max `8,192 B`, 64-bit Gear table).
   - Otherwise: chunk directly via **Gear-hash FastCDC** (min `512 B`, avg `2,048 B`, max `8,192 B`).
   - **Invariant:** Every non-last chunk has length `>= 256 B` (so any search needle `<= 64 B` crosses at most 2 consecutive chunks).
   - **Adversarial Bypass (`W4`):** Track dedup hit rate across the first 64 chunks of a column. If hit rate `< 2%` and compressed ratio `< 1.05`, bypass CDC chunking and emit direct raw/LZ4/Zstd frames (`Codec::None` if incompressible).
3. **Content Addressing:** Hash every chunk with **BLAKE3-128** (first 16 bytes of BLAKE3-256). Deduplicate against the `(project, day)` `ChunkPackBuilder` hash table (`HashMap<[u8; 16], ChunkId>`).

### 5.2 `.stpk` Pack Binary Format

```text
[PackHeader: 32 B]
  magic: b"SPK0", format_version: u16 = 1, flags: u16 (bit 0: ENCRYPTED, bit 1: HAS_DICT, bit 2: HAS_TRIGRAM),
  pack_id: [u8; 16] (UUIDv7), enc_envelope_off: u32, header_crc32c: u32
[Frames 0..F-1]
  Each frame targets 64 KiB raw (or closes early at ~24 KiB estimated compressed).
  1 MiB ReadCache boundary guard: if `cur_off % 1MiB` leaves `< 16 KiB` before next 1 MiB boundary
  and the next frame fits in `< 64 KiB`, zero-pad to the 1 MiB boundary so frames don't straddle blocks.
  Frame header (16 B): magic b"PF", codec u8 (0=None, 1=Lz4, 2=Zstd), flags u8 (bit 0: USES_DICT),
  raw_len u32, stored_len u32, crc32c u32.
[Trigram Skip Section] (optional)
  - Per-frame lowercase trigram bloom filters, sized dynamically:
    `filter_bytes = clamp(next_pow2(unique_trigrams * 10 / 8), 256, min(4096, stored_len / 10))`.
    If `stored_len < 2560` or filter bit density `> 50%` (saturated/incompressible), omit filter (`filter_len = 0`)!
  - Chunk-tail edge table: last `min(63, chunk_len)` bytes of every unique chunk, Zstd-3 compressed.
[DictSection] (optional, 0 B by default)
  Zstd trained dictionary bytes + CRC-32C.
[Optional AEAD Encryption Envelope] (0 B when !ENCRYPTED)
[Pack TOC (zero-parse, contiguous at tail)]
  - FrameTable (`F` × 32 B): `{offset u64, stored_len u32, raw_len u32, codec u8, flags u8, dict_id u16, first_chunk_id u32, chunk_count u32, filter_off u32}`
  - ChunkTable (`C` × 28 B): `{hash [u8; 16], frame_id u32, offset_in_frame u32, raw_len u32}`
[PackPostscript: 64 B]
  toc_len u32, format_version u16, flags u16, frame_count u32, chunk_count u32,
  frame_table_off u32, chunk_table_off u32, trigram_off u64, trigram_len u32,
  edge_table_off u64, edge_table_len u32, dict_off u64, dict_len u32,
  toc_crc32c u32, postscript_crc32c u32, magic b"0KPS".
```

### 5.3 Substring Search (`LIKE` / `ILIKE '%needle%'`)
1. Lowercase `needle` (ASCII fast path via AVX2; Unicode fallback) and extract all 3-byte trigrams (for `len >= 3`; needles of 1–2 bytes scan unique chunks directly without trigram skipping).
2. Check each frame's trigram bloom filter in `TrigramSkipSection` (1 contiguous read). **Skip every frame missing any trigram** (absent needle → 0 frames decompressed!).
3. Decompress ONLY candidate frames; run `memchr::memmem` (or AVX2 ASCII lowercase + `memmem` for `ILIKE`) once per **unique chunk** (`chunk_hit: Bitmap`).
4. Cross-chunk boundary matches (for `needle.len() <= 64`, `k = needle.len() - 1`): for any distinct adjacent pair `(left_id, right_id)` where neither chunk hit individually, check `tail_63(left_id) || head_63(right_id)` using the compact edge table + candidate frame head (or boundary trigram check). For `needle.len() > 64`, fall back cleanly to reconstructing candidate rows whose first chunk or boundary passes.
5. Map matching unique chunks/pairs back to row `Selection` via the compact tail-shared ref lists!

### 5.4 Airtight GDPR Erasure Protocol (`.statam` + `.stpk`)
Given a `(project_id, day)` partition and an erasure predicate (e.g., `user_id = U` or `trace_id IN [...]`):
1. Rewrite every affected `.statam` file excluding erased rows, preserving untouched chunk references temporarily.
2. Scan the chunk-ref columns of **all surviving live `.statam` files** in `(project_id, day)` to build the exact `live_chunks: Bitmap` for each `.stpk` pack (reads ONLY ref lists — zero payload decompression!).
3. For each `.stpk` pack that has **at least one dead chunk** (`live_chunks.count_zeros() > 0`):
   - Create a new immutable `.stpk` pack (`new_pack_id`) and a `chunk_remap: Vec<Option<u32>>`.
   - **Discard any old `DictSection`** unconditionally (prevents erased user substrings from surviving inside a trained dictionary!).
   - For each frame `f` in the old pack:
     - If **0 chunks live**: drop frame `f` completely.
     - If **100% chunks live** AND frame does not depend on a discarded dict: **copy compressed frame bytes verbatim** (zero decompress, zero recompress!) and copy its trigram filter & edge slices verbatim!
     - If **mixed (some live, some dead)** OR depended on a discarded dict: decompress frame `f`, retain **ONLY** live chunks, re-pack and compress surviving chunks into the new pack (recomputing their trigram filter and edge bytes).
   - Remap chunk IDs in the surviving `.statam` files referencing `old_pack_id` -> `new_pack_id` (only rewrites ref columns / pack UUID list), atomically swap catalog references, and **physically delete `old_pack_id.stpk`**.
4. **Verification invariant:** Scanning every byte (both raw and decompressed) of every surviving `.statam` and `.stpk` file on disk after erasure finds **0 bytes** of any erased-only payload.

---

## 6. Asymmetric Write Profiles (`L0Fast` vs `L1Compact`)

| Aspect | `WriteProfile::L0Fast` (Flush) | `WriteProfile::L1Compact` (Compaction) |
|---|---|---|
| **External `.stpk` packs** | **Never** (100% self-contained single file) | **Yes** — per-`(project_id, day)` shared immutable `.stpk` |
| **Narrow integer/time** | `Const`, `Rle`, FastLanes `ForBitpack`/`DeltaBitpack` | `Const`, `Rle`, FastLanes `ForBitpack`/`DeltaBitpack` |
| **Floats (`F64`)** | Fast ALP check (1-pass sample) or raw + `Lz4` | Full ALP + FastLanes FOR + exact bit exceptions |
| **Low-card strings** | Fast hash-dict (capped at 4,096 entries, instant bailout) | Full sorted file-level `Dict` (enables range/prefix code pushdown) |
| **Med/high-card strings** | `Plain` + `Lz4` (with incompressibility guard) | `Fsst` (O(1) random access) or `Zstd(3)` (with guard) |
| **Maps (`MapUtf8`)** | Compact key-dict map + `Lz4` (no shredding) | `ShreddedMap` (keys ≥ 5% presence, ≤ 64 keys) + key-dict residual |
| **Wide payloads** | Inline ≤ 256 B; > 256 B in 64 KiB `Lz4` frames inside RG | Inline ≤ 256 B; > 256 B via Structural JSON / FastCDC + BLAKE3-128 dedup + tail-sharing refs + `.stpk` `Zstd(3)` frames |
| **Bloom filters** | Hot `trace_id` + lazy `session_id`/`user_id`/`score_id` (AVX2 SBBF) | Same (exact NDV sizing) |
| **Trigram skip index** | **Off** (fast flush; L0 files are small and short-lived) | **On** (adaptive ≤ 10% compressed frame, omitted if saturated) |
| **Tiny file fast path** | `<= 16 KiB` raw -> `COMPACT_INLINE_FILE` (single < 64 KiB blob, 1 GET reads entire file) | Same if tiny |

---

## 7. Controlled `unsafe` SIMD Policy & Threat Model

### 7.1 `unsafe` Guardrails
- `crates/statam/Cargo.toml` sets `unsafe_code = "deny"` at crate level; ONLY four dedicated SIMD/tight-loop modules under `crates/statam/src/simd/` carry `#[allow(unsafe_code)]`:
  1. `simd/bitpack.rs` — FastLanes-1024 bit-pack/unpack & parallel delta/FOR (`x86_64` AVX2 + `aarch64` NEON + safe scalar fallback).
  2. `simd/filter.rs` — Vectorized primitive/code predicate evaluation into `Bitmap` words (`x86_64` AVX2 + safe scalar fallback).
  3. `simd/bloom.rs` — 8-lane SBBF block probe & insert (`x86_64` AVX2 + safe scalar fallback).
  4. `simd/trigram.rs` — 32-byte ASCII lowercase & trigram extraction (`x86_64` AVX2 + safe scalar fallback).
- Every `unsafe` block MUST carry a `// SAFETY:` comment proving pointer alignment/bounds/CPU-feature invariants.
- Every SIMD entry point checks `#[cfg(not(miri))]` and runtime feature detection (`is_x86_feature_detected!("avx2")`) and dispatches to the safe scalar fallback otherwise.
- Differential property tests assert **exact bit-for-bit equality** between SIMD and scalar paths across all bit-widths `0..=64`, edge counts (`0, 1, 7, 15, 16, 31, 32, 63, 64, 1023, 1024, 1025, 4096`), and alignments.

### 7.2 Threat Model & Untrusted-Input Bounds (Zero Panics / Zero OOM Aborts)

All files read from disk or S3 are treated as **untrusted adversarial input**. Every length/count is validated against hard caps **before** allocating any buffer:

| Limit | Hard Cap | Rationale |
|---|---|---|
| `MAX_HOT_TAIL_BYTES` | `16 MiB` (writer targets `<= 1 MiB`) | Prevents malicious `hot_tail_len` OOM |
| `MAX_COLUMNS` | `1,024` (incl. shredded sub-cols) | Matches `cetra-column::frame` |
| `MAX_NESTING_DEPTH` | `8` | Matches `cetra_column::MAX_NESTING_DEPTH` |
| `MAX_ROW_GROUPS` | `1,048,576` | Prevents metadata table OOM |
| `MAX_BLOCKS_PER_COL` | `4,194,304` | Bounds `BlockEntry` allocation |
| `MAX_BLOCK_RAW_BYTES` | `64 MiB` | Bounds single block decompression |
| `MAX_FRAME_RAW_BYTES` | `16 MiB` | Bounds `.stpk` frame decompression |
| `MAX_DECOMPRESS_RATIO` | `256× + 4096 B` | Defends against LZ4/Zstd zip bombs |
| `MAX_SHREDDED_KEYS` | `64` per map column | Prevents metadata explosion on `W4` |
| `MAX_REF_CHAIN_DEPTH` | `15` | Prevents malicious cycle/deep-recursion DoS in tail-sharing refs |
| `MAX_CHUNKS_PER_ROW` | `65,536` | Bounds single-row payload reconstruction |
| `MAX_DICT_BYTES` | `32 MiB` | Bounds dictionary allocation |
| `MAX_BLOOM_BYTES` | `32 MiB` | Bounds bloom filter allocation |

All UTF-8 slices are validated via `std::str::from_utf8` (and offset monotonicity/char-boundary invariants checked via `cetra_column::Utf8Data::try_new`). Every block, frame, header, and postscript verifies CRC-32C before parsing. Any violation returns a clean `Err(StatamError::Corrupt { .. })` (feeding `Suspects`/`VerifyQueue` in Stage 3) — never panics, never UB.

---

## 8. Key Decisions & Trade-Offs

| ID | Decision | Rejected Alternative & Why |
|---|---|---|
| D-ST-1 | **PAX 16,384-row Row Groups (Core + Wide-Row) with 4,096-row Mini-Blocks** | Pure column-major (56 disjoint GETs on point lookup fails Gate #4) or pure 4k stripe-major (pulls all wide cols during narrow scan). Two sub-regions per 16k RG hit ≤ 2 GETs on point lookups AND skip wide cols on narrow scans. |
| D-ST-2 | **BLAKE3-128 for `.stpk` chunk IDs + CRC-32C for block/section checksums** | `xxh3-128` (not in `Cargo.lock`, non-cryptographic—vulnerable to adversarial hash collisions on untrusted user payloads). `blake3` (1.8.7) and `crc32c` (0.6.9) are already in `Cargo.lock` and hardware-accelerated. |
| D-ST-3 | **SIMD Split-Block Bloom Filter (SBBF) over Binary Fuse for v0** | Binary Fuse saves ~15% bloom bytes (~1.5 bits/key) but is slower to construct in `L0Fast` and can require retries on construction; SBBF builds in one deterministic pass, probes 8 lanes in 1 AVX2 sequence, and matches Cetra's existing bloom budget. |
| D-ST-4 | **Structural JSON Array Chunker + FastCDC fallback (min 256 B non-last rule)** | Byte-only FastCDC (5× higher CPU, splits inside JSON messages and loses final-message dedup on `]` -> `,` turn growth). Structural chunking gets near-100% dedup on OTel GenAI `input` arrays while FastCDC handles > 8 KiB elements and non-JSON text. |
| D-ST-5 | **Opt-in trained Zstd dicts, unconditionally discarded on GDPR pack rewrite** | Default-on trained dicts (slower encode, negligible gain after dedup, and leaks erased user substrings into dict bytes after GDPR deletion). |
| D-ST-6 | **Meta Nimble-style flat-map shredding capped at 64 keys (≥ 5% presence)** | Unbounded shredding (catastrophic metadata explosion on `W4` 5,000-key maps) or unshredded maps (full map decode on `metadata['k']` like Parquet). |

---

## 9. Milestones (Gated Execution Roadmap)

- **Stage 1 (Current):** `prior-art.md` + `spec-v0.md` (complete binary spec & design doc).
- **Stage 2 (Standalone `crates/statam` + Adversarial Benchmark & Hard Go/No-Go Gate):**
  - **Milestone 2A — Core Container, SIMD Kernels & Narrow Codecs:** `crates/statam` scaffold, error/bounds/CRC32C/header/postscript, AVX2+scalar SIMD kernels (`bitpack`, `filter`, `bloom`, `trigram`), narrow encodings (`Const`, `Rle`, `ForBitpack`, `DeltaBitpack`, `Alp`, `Dict`, `Fsst`, `BitpackBool`, `Plain`, `List`, `Events`, `Links`), and `ShreddedMap`.
  - **Milestone 2B — Payload Layer, `.stpk` Chunk Packs, Substring Search & GDPR:** Structural JSON chunker + FastCDC + BLAKE3-128 dedup + tail-sharing refs, `.stpk` pack writer/reader with 1 MiB boundary alignment, adaptive trigram skip index + 63 B edge table, `L0Fast` & `L1Compact` full `StatamWriter`/`StatamReader` (pruning, late materialization, `Dict`/`Rle`/`Const` pass-through), and `gdpr_rewrite_day` with 0-byte survival proof.
  - **Milestone 2C — Hardening, Fuzzing, Miri & 4-Workload Benchmark Suite:** `fuzz/fuzz_targets/statam_file.rs` + corpus + replay tests, `cargo miri test` verification, `SimulatedS3Reader` (25 ms/GET + 100 MB/s + 1 MiB `ReadCache`), `W1`–`W4` datasets (including real `nebius/SWE-agent-trajectories` & `oasst2` pinned on `precision-5770-vm`), head-to-head benchmark vs `pq_default`/`pq_tuned`/`pq_cetra`, optimization loop until all 5 Go/No-Go gates pass.
- **Stage 3 (Gated on Stage 2 Pass — Not started until user sign-off):**
  - `StatamSource` (`ScanSource`) & `StatamFileWriter` in `crates/storage`, `catalog.db` pack lifecycle, compaction/deletion/verify integration, removal of `arrow` and Parquet storage reads, full `trace_bench`/`ingest_bench`/crash/model/lazyfs/soak/e2e verification, and spec/decision updates (`03-storage.md`, `mvp-decisions.md`).
