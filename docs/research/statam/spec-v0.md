# Statam v0 (`crates/statam`) — Binary Specification & Design Document

**Date:** 2026-11-26  
**Status:** Stage 2 complete — Cold-Reviewed (Passes 1–10) & Gate-Verified (v0.2)  
**Crate:** `crates/statam` (`cetra-statam`)  
**Supersedes:** `docs/research/file-format/prototype-plan.md` (Strata v0)

---

## 1. Goals, Non-Goals & Settled Constraints

### Goals
1. **100% Cetra-native storage format** speaking `cetra-column` (`DataType` — all **16** variants — `Schema`, `Batch`, `Column::{Flat, Const, Dict, Rle}`, `Selection::{All, Mask, Indices}`, `Bitmap`) directly with zero Arrow/DataFusion dependency in the format core.
2. **Beat all three Parquet baselines (`pq_default`, `pq_tuned`, `pq_cetra`) on every Cetra axis:**
   - Strictly smaller on `W1` (agent trajectories), `W2` (multi-turn chat), `W3` (narrow spans/scores), and within ≤ 5% on `W4` (adversarial/incompressible).
   - `WriteProfile::L0Fast` strictly faster than `pq_cetra` on all 4 workloads; `WriteProfile::L1Compact` competitive with or faster than `pq_cetra` at higher compression density.
   - Strictly lower read latency (local warm/cold and simulated S3 with 25 ms/GET + 100 MB/s + exact 1 MiB `ReadCache` block rounding) on narrow scans, aggregates, top-k, `map['key']`, point lookups (`trace_detail`/`span_detail`), and substring search (`LIKE`/`ILIKE`).
   - Cold file open + prune in **1 GET** when compressed Hot Tail `<= DEFAULT_TAIL_HINT` (`128 KiB`), or **2 GETs** when Hot Tail exceeds `128 KiB` (up to `1 MiB` hard Hot Tail budget); point lookup in **≤ 2 GETs** (metadata-cached) and strictly fewer S3 bytes than `pq_tuned` and `pq_cetra` (see exact GET accounting in §3.3 & §5.2).
   - Fast L1 compaction merge (zero recompression of surviving chunks), O(1) day retention drop, and **airtight GDPR erasure** (0 bytes of erased payloads surviving on disk or S3).
3. **Controlled, audited `unsafe` SIMD** inside `crates/statam` only (AVX2 / NEON with safe portable scalar fallback), verified by `cargo miri test` (on all pure-Rust/scalar/SIMD-dispatch tests) and a committed `cargo fuzz` target (`statam_file`), with zero panics, zero OOM aborts, and privacy-safe `Debug`/errors.
4. **Encryption-at-rest readiness:** reserved header/postscript envelope slot for optional per-file / per-pack AEAD encryption (zero byte/CPU overhead when off; non-breaking to enable later).

### Non-Goals
- General-purpose lakehouse/Arrow/DataFusion interop (`.parquet` SQL exports remain a thin low-level writer in `cetra-storage`).
- Cross-day chunk deduplication (packs are strictly scoped to `(project_id, day)`).
- Full-text inverted indexing / Tantivy.
- Modifying `cetra-storage`, `cetra-core`, `cetra-column`, or `cetra-query` before Stage 2 passes all 5 Go/No-Go gates.

---

## 2. High-Level Architecture & Public API

Statam defines **two immutable binary file types** backed by an abstract byte-range reader (`RangeReader` trait supporting slices, local files, and simulated/real S3 + exact 1 MiB `ReadCache`):

1. **Data File (`.statam`):** Stores one sorted table slice (`spans` 56 cols, `traces` 30 cols, `scores` 24 cols, or generic `cetra_column::Schema`) in **PAX Row Groups (131,072 rows, capped at `MAX_ROW_GROUP_SIZE = 262,144`)** composed of **Mini-Blocks (`8,192` rows default in both `L1Compact` and `L0Fast`, capped at `MAX_MINI_BLOCK_SIZE = 65,535`)**, with Core columns tiered at `core_offset` (small analytics -> `start_time`/`created_at` -> `metadata` shredded sub-columns -> ID Core cols -> Tier-2 analytics -> remaining shredded sub-columns -> cold Core cols), and a **Self-Sufficient Hot Tail (LZ4-compressible, `DEFAULT_TAIL_HINT = 128 KiB` default tail fetch, `120 KiB` soft / `1 MiB` hard budget)** containing Postscript, Schema, fixed-width Column Directory, RowGroup & Block Zone Maps, Sort-Key Index, Hot Bloom (`trace_id`), and small File-Level Dictionaries.
   - `WriteProfile::L0Fast`: 100% self-contained single file; fast single-pass encodings (`Const`, `Rle` with 64-element early run sample, FastLanes `ForBitpack`/`DeltaBitpack`, bounded `Dict <= 512`, `Plain` for `MapUtf8`, `Lz4` on `>= 64 B` varlen/nested/`Plain`/`PayloadRefs`, `Codec::None` on fixed-width/`ForBitpack`/`DeltaBitpack`/`BitpackBool`/`Rle`/`Dict`, `Zstd(-5)` on `>= 1 KiB` 512 KiB local payload frames, AVX2 SBBF).
   - `WriteProfile::L1Compact`: Bounded-memory (1-RG streaming, RG0-seeded) full encoding cascade (`Const`, `Rle`, sorted `Dict <= 16,384` for Core / `<= 1,024` & `<= 256 KiB` for non-known-low-card WideRow, FastLanes `DeltaBitpack`/`ForBitpack`, `Alp` with exact decimal division mode, `Fsst` on non-WideRow strings with `512`-row adaptive `LZ4`/`Zstd`/`None` sub-chunks when `row_count > 512 && coded_bytes >= 16 KiB` and within `+10%` of whole-block Zstd, `ShreddedMap`, `Zstd(3)` on non-inline/non-sub-chunked mini-block encodings and shredded sub-columns), adaptive trigram skip indexes, and shared per-`(project_id, day)` immutable **Chunk Packs (`.stpk`)**.
2. **Chunk Pack (`.stpk`, L1 only):** Immutable content-addressed payload store scoped strictly to one `(project_id, day)`, keyed by **BLAKE3-128** (`[u8; 16]`). Supports both `Utf8` text payloads and explicit `DataType::Binary` blob columns in `WriteOptions::payload_cols`. Packs unique text chunks into **`512 KiB` raw `Zstd(3)` frames** by default (automatically adapting down to **`64 KiB`** raw frames after `2 MiB` of unique non-bypass prose when dedup hit rate `< 5%`, mean chunk size `>= 1,024 B`, and `>= 50%` of observed multi-chunk frames had high trigram density `> 0.55` on non-max blooms, `> 0.12` on `16 KiB` max blooms, or a dropped bloom; configurable via `target_frame_raw_bytes`; guarded on `W4` bypass with single-chunk empty `EdgeTable` entries; 1 MiB `ReadCache` boundary-guarded for `< 32 KiB` frames within `< 8 KiB` of boundary), and stores large binary blobs (`>= BLOB_DEDICATED_FRAME_BYTES = 32 KiB`) as dedicated 1-blob-per-frame `PF` frames (chunked at `BLOB_MAX_CHUNK_BYTES = 4 MiB`, with `min_savings_pct = 12` raw bypass for incompressible media and `BLOB_SMALL_FRAME_TARGET_BYTES = 64 KiB` for batched `< 32 KiB` blobs). Includes a compact read-path TOC (LZ4 or Zstd-3 delta-coded `ChunkLoc`s, optional 16 B hashes kept in a separate compaction-only section controlled by `emit_hash_table` so read-path TOC stays tiny!), `TS02` trigram skip section (dynamic 4 KiB / 8 KiB edge tail/head blooms + adaptive per-frame blooms capped at `min(stored_len / 6, clamp(raw_len / 12, 256, 16_384))`, bounded overall by `MAX_BLOOM_BYTES = 32 MiB`), boundary-pruned `head_63 + tail_63` edge table (`edge_table_len == 0` when all rows are single-chunk), and opt-in `PF`-framed Zstd `DictSection` (`ChunkPackBuilder::new_with_dict`, trained once unique raw chunks reach `>= 2 MiB`).

### Public Crate API (`crates/statam`)
- **Profiles & Options:** `WriteProfile::{L0Fast, L1Compact}`, `WriteOptions` (including `mini_block_size = MINI_BLOCK_SIZE = 8_192`, `max_row_group_bytes = 64 MiB`, `write_threads`), `PackWriteOptions` (including `target_frame_raw_bytes = 512 KiB`, `write_threads`, `emit_hash_table`), `TableKind::{Generic, Spans, Traces, Scores}`.
- **Writers:**
  - `StatamWriter::new<W: Write + Seek>(out: W, schema: Arc<Schema>, options: WriteOptions, pack_builder: Option<&mut ChunkPackBuilder>)` -> `write_batch(&mut self, batch: &Batch)` -> `finish(self) -> Result<WriteStats, StatamError>` (streams Row Groups directly to `out`, patches the 32-byte header at `start_offset`, seeks to EOF, and calls `flush()`)
  - `ChunkPackBuilder::new(pack_id: [u8; 16], options: PackWriteOptions)` (plus `enable_boundary_tracking()`) -> `intern_chunk(&mut self, bytes: &[u8]) -> Result<u32, StatamError>` -> `finish<W: Write>(self, out: W) -> Result<PackWriteStats, StatamError>`
- **Readers & I/O:**
  - `RangeReader`: `file_len(&self) -> u64`, `read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError>`, `read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError>`.
  - Built-in impls: `SliceReader`, `FileReader`, `SimulatedS3Reader` (exact `ReadCache` 1 MiB aligned block rounding `(start / 1MiB)..=((end - 1) / 1MiB)` + consecutive-run coalescing + configurable RTT/bandwidth + `IoCounters`).
  - `PackResolver` trait: resolves `pack_id: [u8; 16]` -> `Arc<ChunkPackReader>` (with cached `PackToc`, `FlatEdgeTable`, and `TS02` trigram blooms).
  - `StatamFileMeta::read_from(reader: &dyn RangeReader, tail_hint: Option<u32>) -> Result<Self, StatamError>` (1-GET cold open when Hot Tail `<= tail_hint`, else 2 GETs; validates Postscript CRC-32C + Hot Tail CRC-32C; never needs byte 0 unless byte 0 is already inside the fetched slice or `verify_full` is called).
  - `StatamReader::open(reader, resolver)` / `open_with_meta(reader, meta, resolver)` -> `scan(&self, req: &ScanRequest) -> Result<Vec<Batch>, StatamError>` and `take_rows(&self, row_indices: &[u64], projection: Option<&[usize]>, read_threads: usize) -> Result<Vec<Batch>, StatamError>` (random-access sparse decode).
  - `ScanRequest`: `projection: Option<Vec<usize>>` (`Some(vec![])` emits 0-col row-count batches), `map_keys: Vec<(String, String)>` (prunes projected `MapUtf8` columns to only requested keys when specified), `time_range: TimeRange`, `id_lookups: Vec<IdLookup>`, `predicates: Vec<Predicate>`, `substrings: Vec<SubstringQuery>`, `sort_desc_hint: bool` (enables reverse RG iteration for `start_time DESC LIMIT k`), `limit: Option<usize>`, `coalesce_mode: CoalesceMode::{Auto, Point, Scan}`, `read_threads: usize` (`0`/`1` sequential, clamped to `1..=32`).
- **GDPR Erasure:** `gdpr_rewrite_day(...) -> Result<GdprRewriteReport, StatamError>` (single-rewrite protocol, §5.4).
- **Error Type:** `StatamError` (`thiserror`, privacy-safe `Display`/`Debug`, never prints payload content, IDs, or map values).

---

## 3. Binary Format Specification: `.statam` Data File

All multi-byte integers are **little-endian**. Checksum algorithm is **CRC-32C** (`crc32c`). Raw-block LZ4 uses **`lz4_flex::block`** (no redundant frame header, since block/postscript headers carry exact `raw_len` + `crc32c`).

### 3.1 File Header (32 bytes, offset 0)

| Offset | Size | Field | Value / Semantics |
|---|---|---|---|
| `0..4` | 4 | `magic` | `b"STM0"` |
| `4..6` | 2 | `format_version` | `1` (`u16`) |
| `6..8` | 2 | `flags` | Bitflags (`u16`, identical to Postscript `flags`) |
| `8` | 1 | `table_kind` | `0=Generic, 1=Spans, 2=Traces, 3=Scores` |
| `9` | 1 | `checksum_alg` | `1=CRC32C` |
| `10..12` | 2 | `reserved0` | `0` |
| `12..16` | 4 | `required_features` | `0` in v1 (`u32`, identical to Postscript) |
| `16..24` | 8 | `enc_envelope_offset` | `0` if `!ENCRYPTED` |
| `24..28` | 4 | `enc_envelope_len` | `0` if `!ENCRYPTED` |
| `28..32` | 4 | `header_crc32c` | `crc32c(header[0..28])` |

*Exact sum: `4+2+2+1+1+2+4+8+4+4 = 32 B`.*  
*1-GET invariant:* Cold open reads ONLY the tail (`min(file_len, tail_hint.unwrap_or(128 KiB))`) without straddling 1 MiB `ReadCache` block boundaries. Header fields are cross-checked against Postscript when byte 0 lies inside the fetched tail (small files) or during full `verify()`.

### 3.2 Optional AEAD Encryption Envelope Slot

When `flags & FLAG_ENCRYPTED == 0`: **0 bytes**, `enc_envelope_offset == 0`, `enc_envelope_len == 0`.  
When `flags & FLAG_ENCRYPTED != 0`: `enc_envelope_offset..enc_envelope_offset + enc_envelope_len` (located directly from Postscript without reading byte 0) holds:
`envelope_version: u16 (1)`, `cipher_suite: u16 (1=AES-256-GCM, 2=XChaCha20-Poly1305)`, `key_id_len: u16`, `wrapped_dek_len: u16`, `nonce_prefix: [u8; 12]`, `aad_binding: [u8; 32]`, `key_id`, `wrapped_dek`, `envelope_crc32c: u32`. Per-block/frame nonces derive from `nonce_prefix || id` with a 16-byte auth tag per block/frame. Readers without a key return `Err(StatamError::EncryptionRequired)`.

### 3.3 Postscript (112 bytes, final 112 bytes of file)

| Offset | Size | Field | Description |
|---|---|---|---|
| `0..4` | 4 | `hot_tail_len` | Stored bytes of Hot Tail **including** this 112 B Postscript (`>= 112`) |
| `4..8` | 4 | `hot_tail_raw_len` | Uncompressed length of `hot_tail_body` (`== hot_tail_len - 112` if `!HOT_TAIL_LZ4`) |
| `8..10` | 2 | `format_version` | `1` |
| `10..12` | 2 | `flags` | `0x0001 ENCRYPTED`, `0x0002 L1_COMPACT`, `0x0004 HAS_PACK_REFS`, `0x0008 HAS_SHREDDED_MAPS`, `0x0010 HAS_TRIGRAM_INDEX`, `0x0020 HOT_TAIL_LZ4` |
| `12..16` | 4 | `required_features` | `0` in v1 (reader rejects unknown nonzero bits) |
| `16..24` | 8 | `row_count` | Total rows (`<= MAX_ROWS_PER_FILE = u32::MAX as u64`) |
| `24..28` | 4 | `num_logical_cols` | Logical columns (`<= MAX_COLUMNS = 1,024`) |
| `28..32` | 4 | `num_phys_cols` | Logical + shredded map sub-columns (`<= MAX_COLUMNS = 1,024`) |
| `32..36` | 4 | `num_row_groups` | Count of `RowGroupEntry` (`G <= MAX_ROW_GROUPS`) |
| `36..40` | 4 | `total_blocks` | Count of `BlockEntry` (`<= MAX_TOTAL_BLOCKS`) |
| `40..44` | 4 | `schema_off` | Offset in uncompressed Hot Tail body |
| `44..48` | 4 | `schema_len` | Byte length of Schema section |
| `48..52` | 4 | `col_dir_off` | Offset of `[ColumnDirEntry; num_phys_cols]` (`len == num_phys_cols * 56`) |
| `52..56` | 4 | `rg_dir_off` | Offset of `[RowGroupEntry; num_row_groups]` (`len == num_row_groups * 48`) |
| `56..60` | 4 | `block_dir_off` | Offset of `[BlockEntry; total_blocks]` (`len == total_blocks * 64`) |
| `60..64` | 4 | `hot_bloom_off` | Offset of Hot Bloom bytes in Hot Tail body (`0` if `hot_bloom_len == 0`) |
| `64..68` | 4 | `hot_bloom_len` | Byte length of Hot Bloom section |
| `68..72` | 4 | `hot_dict_off` | Offset of Hot Dict bytes in Hot Tail body (`0` if `hot_dict_len == 0`) |
| `72..76` | 4 | `hot_dict_len` | Byte length of Hot Dict section |
| `76..80` | 4 | `lazy_dir_off` | Offset of Lazy Index Directory in Hot Tail body (`0` if `lazy_dir_len == 0`) |
| `80..84` | 4 | `lazy_dir_len` | Byte length of Lazy Index Directory |
| `84..88` | 4 | `enc_envelope_len` | Byte length of AEAD envelope (`0` when `!ENCRYPTED`) |
| `88..96` | 8 | `enc_envelope_offset` | Absolute offset of AEAD envelope (`0` when `!ENCRYPTED`) |
| `96..100` | 4 | `hot_tail_crc32c` | `crc32c` over stored `hot_tail_body` (`[0 .. hot_tail_len - 112]`) |
| `100..104` | 4 | `reserved` | `0` |
| `104..108` | 4 | `postscript_crc32c` | `crc32c(postscript[0..104])` |
| `108..112` | 4 | `magic` | `b"0MTS"` |

*Exact sum: `4+4+2+2+4+8+4+4+4+4+4+4+4+4+4+4+4+4+4+4+4+4+8+4+4+4+4 = 112 B`.*  
**Why `HOT_TAIL_LZ4` is decisive:** Compressing `hot_tail_body` with `lz4_flex::block` (when `compressed + 16 < raw`) shrinks a 1M-row `spans` metadata tail from ~1.5 MiB down to **~250–450 KiB** (guaranteed `<= 1 MiB` -> **1 GET**) AND shrinks a tiny 1-row `W4` flush tail from ~7.4 KiB down to **~1.2 KiB**, decompressing at >3 GB/s (~0.1 ms).

### 3.4 Hot Tail Sections (inside uncompressed `hot_tail_body`)

1. **Schema & Table Metadata (`schema_off..schema_off + schema_len`):**
   - `table_kind: u8`, `sort_col_count: u8`, `sort_cols: [u16; sort_col_count]`
   - `pack_ref_count: u16`, `pack_ids: [[u8; 16]; pack_ref_count]`
   - `logical_field_count: u16`, each field: `name_len: u16`, `name_utf8`, `type_encoding`, `nullable: u8`, `locality_group: u8` (`0=Core, 1=WideRow, 2=ExternalPayload`).
   - Exact `DataType` `type_tag: u8` encoding (all 16 variants, validated with `DataType::validate(MAX_NESTING_DEPTH)` on read):
     - `0 = Null`, `1 = Bool`, `2 = I64`, `3 = F64`, `4 = Decimal128` (+ `precision: u8, scale: i8`), `5 = TimestampNs` (+ `utc: u8`), `6 = Date`, `7 = Interval`, `8 = Utf8`, `9 = Binary`, `10 = FixedBytes8`, `11 = FixedBytes16`, `12 = List` (+ recursive `type_encoding`), `13 = MapUtf8`, `14 = Events`, `15 = Links`.
   - `shredded_map_count: u16`, each: `logical_col_idx: u16`, `key_count: u16` (`<= 64`), each key: `key_len: u16`, `key_utf8`, `sub_col_idx: u16`, `residual_count: u32`.

2. **Column Directory (`col_dir_off`, `num_phys_cols` × **56 B**):**
   - `0..2`: `phys_col_id: u16`, `2..4`: `logical_col_id: u16`, `4`: `locality_group: u8`
   - `5`: `col_flags: u8` (`0x01 HAS_FILE_DICT`, `0x02 DICT_SORTED`, `0x04 IS_PAYLOAD_REFS`, `0x08 IS_SHREDDED_SUBCOL`, `0x10 DICT_IN_HOT_TAIL`, `0x20 BLOOM_IN_HOT_TAIL`, `0x40 HAS_TRIGRAM_INDEX`)
   - `6..8`: `reserved: u16`
   - `8..12`: `num_blocks: u32`, `12..16`: `first_block_idx: u32`
   - `16..24`: `dict_offset: u64` (relative to `hot_dict_off` if `DICT_IN_HOT_TAIL`, else file-absolute), `24..28`: `dict_len: u32`, `28..32`: `dict_entry_count: u32`
   - `32..40`: `bloom_offset: u64` (relative to `hot_bloom_off` if `BLOOM_IN_HOT_TAIL`, else file-absolute), `40..44`: `bloom_len: u32`, `44..48`: `null_count_total: u32`
   - `48..56`: `trigram_offset: u64` (file-absolute offset of narrow-column trigram section, e.g. `spans.name`; `0` if none)

3. **Row-Group Directory (`rg_dir_off`, `num_row_groups` × **48 B**):**
   - `0..4`: `first_row: u32`, `4..8`: `row_count: u32` (default `131,072`, `<= MAX_ROW_GROUP_SIZE = 262,144`)
   - `8..16`: `core_offset: u64`, `16..20`: `core_len: u32`
   - `20..24`: `wide_len: u32`, `24..32`: `wide_offset: u64` (`wide_offset == core_offset + core_len` — Core + Wide are strictly contiguous!)
   - `32..40`: `l0_payload_offset: u64`, `40..44`: `l0_payload_len: u32` (immediately after `wide_offset + wide_len` in `L0Fast`; `0` in `L1Compact`)
   - `44..48`: `rg_flags: u32` (`0` in v1)

4. **Mini-Block Directory (`block_dir_off`, `total_blocks` × **64 B**):**
   - `0..4`: `first_row: u32`, `4..6`: `row_count: u16` (`8,192` default in `L1Compact` and `L0Fast`, `<= MAX_MINI_BLOCK_SIZE = 65,535`), `6..8`: `null_count: u16`
   - `8..16`: `offset: u64` (`0` for inline `Const`/`ConstNull`)
   - `16..20`: `stored_len: u32`, `20..24`: `raw_len: u32`
   - `24`: `encoding: u8`, `25`: `codec: u8` (`0=None, 1=Lz4, 2=Zstd`), `26`: `block_flags: u8` (`0x01 HAS_NULLS`, `0x02 ALL_NULL`, `0x04 MIN_MAX_VALID`, `0x08 HAS_NAN`, `0x10 CONST_EMPTY_COLLECTION`, `0x20 FSST_SUBCHUNKED` — writer emits `outer codec = Codec::None`, while `BlockEntry::decode` enforces `encoding == Encoding::Fsst` and `codec == Codec::None => raw_len == stored_len`), `27`: `min_inline_len: u8` (`0..=16`), `28`: `max_inline_len: u8` (`0..=16`), `29..32`: `reserved: [u8; 3]`
   - `32..48`: `min_inline: [u8; 16]`, `48..64`: `max_inline: [u8; 16]`

   **Zone-Map Exactness & Pruning Invariants:**
   - `I64` / `TimestampNs`: 8 B LE in `[0..8]`, `min_inline_len = max_inline_len = 8`.
   - `Date`: 4 B LE in `[0..4]`, `len = 4`.
   - `Decimal128`: full 16 B `i128` LE in `[0..16]`, `len = 16`.
   - `FixedBytes8` / `FixedBytes16`: exact 8 B / 16 B lexicographic min/max, `len = 8 / 16`.
   - `F64`: `NaN` excluded from `min_inline`/`max_inline` and sets `HAS_NAN = 1`; if all non-null values are `NaN`, `MIN_MAX_VALID = 0` (`HAS_NAN = 1`). `-0.0` canonicalized to `+0.0`. Under `cetra-query`'s total order (`NaN > +inf`): when `HAS_NAN == 1`, NEVER prune `>`, `>=`, `!=`, `= NaN`, or `IS NOT NULL`; only prune `< x`, `<= x`, `= x` (finite `x`) when `MIN_MAX_VALID == 1 && x < min`.
   - `Utf8` / `Binary`: compared strictly as raw byte slices `&min_inline[..min_inline_len]` and `&max_inline[..max_inline_len]`. If true max length `> 16`, compute 16-byte upper bound by incrementing byte 15 with left-carry; if all 16 bytes are `0xFF`, clear `MIN_MAX_VALID = 0`.
   - `Dict`: 4 B LE `u32` code bounds in `[0..4]`. Range/prefix pruning (`<`, `<=`, `>`, `>=`, `BETWEEN`, `LIKE 'p%'`) is **ONLY allowed when `col_flags & DICT_SORTED != 0`**; unsorted `L0Fast` dicts may prune `=`/`IN` only.
   - `Interval` / `List` / `MapUtf8` / `Events` / `Links`: `MIN_MAX_VALID = 0`.

5. **Bloom Filter Wire Format (SBBF) & Deterministic Hash:**
   - Single-filter SBBF (`BF01`): **12 B header**: `magic: b"BF01"`, `num_blocks: u32` (each block = 32 B = 8 × `u32`), `crc32c: u32` (over the `num_blocks * 32` bytes), followed by `[u32; num_blocks * 8]` LE.
   - Per-Row-Group SBBF (`BF02`, emitted by `StatamWriter` for per-RG pruning on unsorted columns): **12 B header**: `magic: b"BF02"`, `num_rgs: u32` (`<= MAX_ROW_GROUPS`), `crc32c: u32` (over `[u32; num_rgs] || concat(BF01 bytes)`), followed by `[u32; num_rgs]` (byte length of each RG's `BF01`) and `concat(BF01 bytes)`. Readers accept both `BF01` and `BF02`.
   - **Deterministic 64-bit hash (`statam_bloom_hash(bytes: &[u8]) -> u64`):** Fixed WyHash/SplitMix64-grade pure-Rust mixer with golden constants (defined in full in Phase 1).
   - **Columns getting SBBF blooms (FPP = 0.01, `max_ndv = rows`):**
     - `spans`: `trace_id` (Hot Tail), `span_id`, `session_id`, `user_id` (Hot Tail if `hot_tail_len <= 112 KiB`, else contiguous Lazy Bloom Section).
     - `traces`: `trace_id` (Hot Tail), `session_id`, `user_id`.
     - `scores`: `trace_id` (Hot Tail), `score_id`.
   - **Lazy Index Directory (`lazy_dir_off`):** Array of **24 B entries** `{kind: u8 (1=Bloom, 2=Dict, 3=NarrowTrigram), reserved: [u8; 3], phys_col_id: u32, offset: u64, len: u32, crc32c: u32}`.

6. **File-Level Dictionary Wire Format:**
   - **20 B header:** `magic: b"DT01"`, `entry_count: u32`, `codec: u8 (0=None, 1=Lz4, 2=Zstd)`, `value_kind: u8 (0=Utf8, 1=Binary, 2=Fixed8, 3=Fixed16)`, `reserved: u16`, `raw_len: u32`, `crc32c: u32` (20 B total), followed by body (`[u32; entry_count + 1]` offsets + bytes for var-width, or fixed bytes).
   - **Pass-through cap:** `StatamReader` emits `Column::Dict` directly when `dict_raw_bytes <= 256 KiB` (avoiding cloning multi-MB dicts per 8k mini-block); larger dicts materialize per mini-block.

### 3.5 Data Block Framing (16 B Header)
Every non-inline data block starts with **16 B**: `magic: b"SB"`, `encoding: u8`, `codec: u8`, `raw_len: u32`, `stored_len: u32`, `crc32c: u32` (header-inclusive CRC-32C over header prefix `frame[0..12]` followed by the `stored_len` bytes after the header). Runtime readers validate `encoding`, `codec`, and `raw_len` against the corresponding `BlockEntry` and return `Cow::Borrowed` zero-copy when `codec == Codec::None`.  
**Incompressibility & minimum-savings guard:** Store compressed ONLY if `compressed.len() + 16 < raw.len()`, within `MAX_DECOMPRESS_RATIO`, and (when `min_savings_pct > 0 && raw.len() >= 1024`, e.g. Pass 11's `12%` threshold on high-entropy numeric/varlen blocks) saving at least `min_savings_pct` percent; else `codec = 0 (None)`.

---

## 4. Narrow & Structured Column Encoding Suite

| Code | Encoding | `DataType`s | Output `Column` | Wire Body & Invariants |
|---|---|---|---|---|
| `0` | `ConstNull` | All | `Column::Const(Null)` | 0 B in data region (`ALL_NULL = 1`). |
| `1` | `Const` | Scalars (≤ 16 B) or empty collections (`CONST_EMPTY_COLLECTION=1`) | `Column::Const(v)` | 0 B in data region; value in `min_inline[..min_inline_len]`; requires `null_count == 0`. |
| `2` | `Rle` | All scalars, `FixedBytes8/16`, `Interval` | `Column::Rle` | `num_runs: u32`, run-level validity bitmap (`num_runs.div_ceil(8)` B if `HAS_NULLS`), FastLanes-packed `run_ends: [u32; num_runs]`, encoded run values (using inner scalar encoding). |
| `3` | `ForBitpack` | `I64`, `TimestampNs`, `Date`, `Decimal128` (when `max-min <= u64::MAX`) | `Column::Flat` | Optional row null bitmap (`row_count.div_ceil(8)` B), `base: i64` (or `i128` for `Decimal128`), `bit_width: u8`, FastLanes-1024 full tiles (`count / 1024`) + compact scalar bitpack tail (`count % 1024` — zero padding!). |
| `4` | `DeltaBitpack` | `TimestampNs`, `I64`, `Date` | `Column::Flat` | Optional row null bitmap, `is_zigzag: u8`, per-1024-tile bases + `bit_width: u8` + FastLanes-1024 `04261537` transposed deltas + scalar tail (`count % 1024`). |
| `5` | `Alp` | `F64` | `Column::Flat(F64)` | Optional row null bitmap, `exp: u8` (`exp & 0x7F <= 18`, bit 7 `ALP_DIV_MODE_FLAG = 0x80` = exact division mode, `div_mode && (exp & 0x7F) == 0` invalid), `fac: u8`, `exc_count: u16`, `ForBitpack` of scaled `i64`s, `[u16; exc_count]` positions, `[u64; exc_count]` exact IEEE-754 bits (`NaN`, `±inf`, `-0.0` bit-exact!). |
| `6` | `Dict` | `Utf8`, `Binary`, `FixedBytes8/16` | `Column::Dict` (≤ 256 KiB dict) or `Flat` | Optional row null bitmap, `sub_enc: u8 (0=ForBitpack, 1=Rle)` over `u32` codes into the column's file-level dictionary. |
| `7` | `Fsst` | `Utf8`, `Binary` | `Column::Flat` | Optional row null bitmap, `sym_count: u8 (<=255)`, `[u8; sym_count]` symbol lengths (1..=8), concatenated symbol bytes. **Single-chunk (`FSST_SUBCHUNKED = 0`):** `len_bw: u8`, FastLanes-packed compressed string lengths (`u32`), concatenated FSST-coded bytes (`0xFF` = literal escape). **Sub-chunked (`FSST_SUBCHUNKED = 0x20`, emitted when `row_count > 512 && coded_bytes >= 16 KiB` and, on `L1Compact`, `subchunked_len * 100 <= whole_zstd_len * 110`; writer emits outer `SB` frame with `Codec::None` and `raw_len == stored_len`, including enclosing `ListEncoded`/`ShreddedSubCol`):** `subchunk_rows: u16` (`1..=8192`, writer uses `512`), `num_chunks: u16` (`== row_count.div_ceil(subchunk_rows)`), `[codec: u8, raw_len: u32, stored_end: u32; num_chunks]` (9 B/entry; per-sub-chunk adaptive `Lz4`/`Zstd`/`None`, monotonic `stored_end` ending exactly at payload length, `sum(raw_len) <= MAX_BLOCK_RAW_BYTES`, per-sub-chunk decompression ratio bounds), concatenated sub-chunk payloads each decompressing to `len_bw: u8` + FastLanes `pack_u32(sub_row_lens)` + `sub_coded_bytes`. |
| `8` | `BitpackBool` | `Bool` | `Column::Flat(Bool)` | Optional row null bitmap, exact `row_count.div_ceil(8)` packed LE bytes (matches `Bitmap::from_bytes`). |
| `9` | `Plain` | All fixed/var types (incl. `Interval`, `Decimal128`, `W4`) | `Column::Flat` | Optional row null bitmap, raw fixed bytes or `(offsets: [u32; row_count+1], data_bytes)` + optional `Lz4`/`Zstd`. |
| `10` | `ListEncoded` | `List(T)` | `Column::Flat(List)` | Optional row null bitmap, FastLanes-packed list lengths (`u32`), recursively encoded child block (e.g. `Dict` for `tags`/`models`/`prompts`/`conventions`!). |
| `11` | `ShreddedMap` | `MapUtf8` (`L1Compact`) | `Column::Flat(MapUtf8)` (or pruned map for `map_keys`) | Residual block stores optional row null bitmap + compact key-dict `(offsets, key_codes, values, value_nulls)` for rare & duplicate keys ONLY. Each promoted key (`<= 64`, `>= 5%` presence) is a physical sub-column storing `in_map: Bitmap` + `value_null: Bitmap` + typed value block (`Const`/`Rle`/`Dict`/`Fsst`/`Plain`). Null values of promoted keys stay in the shredded sub-column so `residual_count == 0`! |
| `12` | `EventsStruct` | `Events` | `Column::Flat(Events)` | Optional row null bitmap, FastLanes list lengths, `DeltaBitpack` `times`, `Dict`/`Plain` `names`, `Fsst`/`Plain` `attributes` + `attr_nulls` bitmap. |
| `13` | `LinksStruct` | `Links` | `Column::Flat(Links)` | Optional row null bitmap, FastLanes list lengths, raw `trace_ids`, raw `span_ids`, `Fsst`/`Plain` `attributes` + `attr_nulls` bitmap. |
| `14` | `PayloadRefs` | Heavy `Utf8` & explicit `Binary` (`WriteOptions::payload_cols`) | `Column::Flat(Utf8 \| Binary)` | Optional row null bitmap, followed by row-tag stream + inline bytes + tail-sharing chunk refs: tag byte per non-null row (`0 = inline [u16 len + bytes]`, `1 = keyframe [u16 n_chunks + (u16 pack_idx, u32 chunk_id) * n]`, `2 = delta [u8 back (1..=32), u16 keep_prefix, u16 n_new + delta-varint (pack_idx, chunk_id)]`, chain depth `<= 15`). In `L0Fast`, `pack_idx = 0xFFFF` addresses local RG payload frames in `l0_payload_offset..`. Large `Binary` rows (`>= BLOB_DEDICATED_FRAME_BYTES = 32 KiB`) split at `BLOB_MAX_CHUNK_BYTES = 4 MiB` into dedicated 1-blob-per-frame `PF` frames (`min_savings_pct = 12` raw bypass, no CDC/dedup/trigram/EdgeTable); `< 32 KiB` binary blobs batch into `BLOB_SMALL_FRAME_TARGET_BYTES = 64 KiB` bypass frames. |

---

## 5. Wide-Payload Layer, `.stpk` Packs, Substring Search & GDPR

### 5.1 Chunking (`L1Compact`) & Strict `>= 256 B` Non-Last Invariant
- `len <= 256 B`: stored inline inside `PayloadRefs` (`tag = 0`).
- `len > 256 B`:
  - Leading `[` (after ASCII whitespace): **Structural JSON Array Chunker** cuts at top-level element boundaries once accumulated chunk `>= 256 B`. Splits trailing `]` ONLY when the current accumulated chunk ALREADY has `>= 256 B`. Any single element `> 8,192 B` (or malformed unclosed quote exceeding `8,192 B`) is sub-chunked with **FastCDC (Gear hash, min 512 B, avg 2,048 B, max 8,192 B)**.
  - **Strict non-last length invariant:** NEVER emit a non-last chunk `< 256 B`. When sub-chunking a slice of length `L`, if `L > 8,192 && L <= 8,192 + 256`, cut at `L - 256`; if an element's final remainder is `< 256 B` and more elements follow, carry it into the next element's accumulator. Result: every non-last chunk has `len >= 256 B`, guaranteeing any `<= 64 B` needle spans **at most 2 adjacent chunks**.
  - **Adversarial `W4` bypass:** If the first 64 chunks of a column have `< 2%` dedup hit rate and `< 1.05×` compressibility, bypass CDC hashing, BLAKE3 dedup, and trigram extraction, omit redundant `EdgeTable` bytes for single-chunk payloads, and emit direct `Zstd`-guarded frames (auto-falling back to `Codec::None` if incompressible).
- **Content hash:** `BLAKE3-128` (first 16 bytes of `blake3::hash(chunk)`).

### 5.2 `.stpk` Pack Binary Format

```text
[PackHeader: 48 B]
  magic: b"SPK0", format_version: u16=1, flags: u16, required_features: u32=0,
  pack_id: [u8; 16], enc_envelope_offset: u64, enc_envelope_len: u32,
  reserved: u32=0, header_crc32c: u32

[Frames 0..F-1]  (target 512 KiB raw Zstd(3) by default, adapting to 64 KiB for unique high-density prose after 2 MiB;
  large Binary blobs >= BLOB_DEDICATED_FRAME_BYTES (32 KiB) get dedicated 1-blob-per-frame PF frames up to
  BLOB_MAX_CHUNK_BYTES (4 MiB) with 12% min-savings raw bypass, and < 32 KiB blobs target BLOB_SMALL_FRAME_TARGET_BYTES (64 KiB);
  1 MiB ReadCache boundary guard for < 32 KiB frames:
  if cur_off % 1MiB leaves < 8 KiB before next 1 MiB boundary and frame < 32 KiB, pad to 1 MiB)
  FrameHeader (16 B): magic b"PF", codec u8 (0=None,1=Lz4,2=Zstd), flags u8 (0x01 USES_DICT),
  raw_len u32, stored_len u32, crc32c u32 (header-inclusive CRC-32C over header[0..12] || stored)

[ChunkHashTable] (optional compaction-only: [[u8; 16]; chunk_count] || crc32c: u32, NEVER read by queries;
  omitted with `hash_table_off = 0` when `!PackWriteOptions::emit_hash_table || chunk_count == 0`)
[TrigramSkipSection] (optional, `TS02` header:
  magic: b"TS02", edge_bloom_bytes: u32 (4096 if stored_frame_bytes < 2 MiB else 8192),
  crc32c: u32 (CRC-32C over header[0..8] || section[12..]),
  edge_tail_bloom: [u8; edge_bloom_bytes] (ASCII-lowercased trigrams of every chunk's tail_63),
  edge_head_bloom: [u8; edge_bloom_bytes] (ASCII-lowercased trigrams of every chunk's head_63),
  followed by per-frame lowercase ASCII trigram bloom filters with `filter_off` relative to section start;
  cap = clamp(min(stored_len / 6, clamp(raw_len / 8, 256, 16_384)), 256, 16_384) rounded down to power of 2;
  omitted (filter_len = 0) if stored_len < 1024, bit density > 72%, or section would exceed MAX_BLOOM_BYTES; `TS02` magic required when `trigram_len > 0`)
[EdgeTable] (`PF`-framed Zstd-3 compressed payload capped at `MAX_BLOCK_RAW_BYTES = 64 MiB`: per unique chunk [u8 head_len, head_bytes (<=63), u8 tail_len, tail_bytes (<=63)];
  when boundary tracking is enabled, omitted (`edge_table_len = 0`) if all rows are single-chunk, and non-boundary
  chunks store zero-length edges on mixed packs; multi-chunk rows referencing an `edge_table_len == 0` pack return `Corrupt`)
[DictSection] (opt-in via `ChunkPackBuilder::new_with_dict`, 0 B by default or when unique raw chunks < 2 MiB; otherwise a `PF`-framed Zstd-3 compressed
  64 KiB dictionary trained once unique raw chunks reach >= 2 MiB from <= 1,024 uniformly strided
  <= 2,048 B chunk samples, placed right before Pack TOC Body with `PACK_FLAG_HAS_DICT = 0x0008`)
[Optional AEAD Envelope] (0 B when !ENCRYPTED)
[Pack TOC Body] (LZ4-compressed when `PACK_FLAG_TOC_LZ4` set, or Zstd(3) delta-coded `ChunkLoc` offsets when `PACK_FLAG_TOC_ZSTD = 0x0010` set):
  [FrameEntry; frame_count] (40 B each: offset:u64, stored_len:u32, raw_len:u32, codec:u8, flags:u8,
    dict_id:u16, first_chunk_id:u32, chunk_count:u32, filter_off:u32, filter_len:u32, crc32c:u32)
  [ChunkLoc; chunk_count]   (12 B each: frame_id:u32, offset_in_frame:u32, raw_len:u32)
[PackPostscript: 112 B]
  toc_stored_len:u32 (incl. 112 B), toc_raw_len:u32, format_version:u16, flags:u16,
  required_features:u32, pack_id:[u8;16], frame_count:u32, chunk_count:u32,
  hash_table_off:u64, trigram_off:u64, trigram_len:u32, edge_table_len:u32, edge_table_off:u64,
  dict_off:u64, dict_len:u32, enc_envelope_offset:u64, enc_envelope_len:u32,
  reserved:u32, toc_crc32c:u32, postscript_crc32c:u32, magic:b"0KPS"
```
*Exact PackPostscript sum: `4+4+2+2+4+16+4+4+8+8+4+4+8+8+4+8+4+4+4+4+4 = 112 B`.*

**Exact S3 GET Budget Analysis (Gate #4):**
- **Cold `.statam` open + prune:** **1 GET** when compressed Hot Tail `<= 128 KiB` (`min(file_len, 128 KiB)` reads LZ4-compressed Hot Tail without straddling 1 MiB `ReadCache` block boundaries), or **2 GETs** for larger files whose Hot Tail exceeds `128 KiB` (up to `1 MiB` hard budget).
- **Point lookup (`trace_detail` narrow / `span_detail` / L0 full):** **≤ 2 GETs** (GET #1 Hot Tail, GET #2 contiguous `core_offset..wide_offset+wide_len` RG slice ≤ 1 MiB).
- **Point lookup (`trace_detail` full `SELECT *` on L1 with external `.stpk`):**
  - Steady state (`.statam` Hot Tail + day's `.stpk` Pack TOC cached in `MetadataCache`): **1 GET** on `.statam` RG + **1 coalesced GET** on `.stpk` frames = **2 GETs**, ~0.2–0.8 MiB total (vs `pq_cetra` 74 GETs / 2.6 MiB and `pq_tuned` 101 GETs / 3.4 MiB).
  - Completely cold (no metadata cache): 1 GET `.statam` tail + 1 GET `.statam` RG + 1 GET `.stpk` TOC + 1 GET `.stpk` frames (still 25× fewer GETs and 3–5× fewer bytes than Parquet; both cold and metadata-cached numbers are reported explicitly in the Stage 2 benchmark!).

### 5.3 Exact Substring Search (`LIKE` / `ILIKE '%needle%'`)
1. **Inline values (`<= 256 B`):** Always scanned directly in `PayloadRefs` blocks (zero missed short matches; decoded into zero-per-row-allocation CSR `FlatPayloadRefs`).
2. **Narrow `Utf8` columns (e.g. `spans.name`):** Phase-1 `LIKE`/`ILIKE` evaluates directly on raw bytes without `from_utf8` validation (`Const` once, `Dict` once per dictionary entry, `Rle` once per unique run, `Fsst` via reusable `FsstBlockView` scratch buffer, `Plain` via raw slice scan); blocks with `HAS_TRIGRAM_INDEX` are pruned via per-block trigram filters in `LazyIndexDir`.
3. **External `.stpk` payloads:**
   - **ASCII trigram extraction:** On both build and query, `simd/trigram.rs` performs **exact ASCII-only lowercasing** (`A..Z -> a..z`, non-ASCII untouched). For `ILIKE`, sound ASCII-run extraction (`extract_ilike_trigram_hashes`) extracts trigrams from contiguous ASCII segments excluding characters with cross-ASCII Unicode case folds (`K`/`k` ↔ `K`, `S`/`s` ↔ `ſ`, `I`/`i` ↔ `İ`), allowing even multilingual Unicode `ILIKE` needles (e.g. `"Alienación"`) to prune `.stpk` frames with 0 false negatives (if no safe trigrams exist or `needle.len() < 3`, skip bloom pruning and scan all unique chunks).
   - **Case `needle.len() <= 64` (and `max_unicode_haystack_match_bytes <= 64` for Unicode `ILIKE`):**
     - **Step A (Interior matches):** Decompress ONLY frames whose trigram bloom matches all extracted trigrams (`filter_len == 0` frames are always scanned). Scan each unique chunk ONCE via `memchr::memmem` (or AVX2 ASCII lowercase + `memmem` for ASCII `ILIKE`, Unicode `to_lowercase()` for non-ASCII `ILIKE`).
     - **Step B (Boundary matches — Pigeonhole skip + 0 extra frame decompression!):** For `needle.len() >= 5`, check `edge_tail_bloom` for trigrams in the needle's prefix half and `edge_head_bloom` for trigrams in its suffix half (sound for both `LIKE`, ASCII `ILIKE`, and Unicode `ILIKE`). If BOTH reject (`!tail_may && !head_may`), no match can cross any chunk boundary by the Pigeonhole Principle — skip `EdgeTable` and all boundary pair loops completely! Otherwise, check every distinct adjacent chunk pair `(left, right)` in surviving rows using the cached zero-allocation `FlatEdgeTable`'s `tail_63(left) || head_63(right)`. Guarantees **0 false negatives** even when `needle` straddles two chunks whose frames were both skipped in Step A!
   - **Case `needle.len() > 64` (or Unicode `ILIKE` worst-case haystack match `> 64 B`):** Reconstruct payloads for surviving rows (still deduplicating identical chunk-ref sequences) and run exact `memmem`/Unicode search — zero false negatives!

### 5.4 Airtight Single-Rewrite GDPR Erasure Protocol
1. Identify affected `.statam` files in `(project_id, day)` and evaluate the erasure predicate on surviving rows.
2. Collect the exact `live_chunks: Bitmap` for each `.stpk` pack across ALL surviving rows of ALL live `.statam` files in `(project_id, day)` (reads ONLY ref lists — zero payload decompression).
3. For each `.stpk` pack with any dead chunk:
   - Write `new_pack_id.stpk` FIRST, producing `chunk_remap: Vec<Option<u32>>`.
   - Unconditionally discard any old `DictSection`!
   - 100%-live frames (not using a discarded dict): **copy compressed frame bytes verbatim** (zero decompress/recompress) + copy trigram filter & edge entries.
   - Mixed frames (some live, some dead) or frames using a discarded dict: decompress, retain ONLY live chunks, re-compress and recompute trigram filter & edge entries.
   - 0%-live frames: drop completely.
4. Rewrite every `.statam` file referencing `old_pack_id` **ONCE** (dropping erased rows from affected files and applying `chunk_remap` + `new_pack_id` in one pass).
5. Atomically swap metadata references, **evict `old_pack_id.stpk` from `ReadCache` / local cache**, and physically delete `old_pack_id.stpk`. **0 bytes of erased-only payloads survive.**

---

## 6. Streaming Writer Memory & Profile Matrix

**Bounded `L1Compact` Streaming:** `StatamWriter<'a, W: Write + Seek>` buffers at most **1 Row Group** (`<= row_group_size` rows and `<= max_row_group_bytes`, default `131,072` rows / `64 MiB` encoded/stored bytes) and streams each flushed RG directly into `W` without retaining previous RG blobs in RAM. RG0 determines the `ShreddedMap` promoted keys (`<= 64`, `>= 5%` presence) and initial sorted file-level dicts (`<= 16,384` entries). In RG1..G-1, unseen map keys spill to residual (updating `residual_count`), and unseen string values in dict-encoded columns fall back to `Fsst`/`Plain` for that mini-block — guaranteeing bounded O(1 RG) RAM even on multi-hundred-MB compactions.

*(Profile table: `L0Fast` = self-contained, FastLanes + Rle (64-element sample) + Const + bounded hash-dict ≤ 512 + Plain MapUtf8 + `Lz4` on `>= 64 B` varlen/nested/`Plain`/`PayloadRefs` + `Zstd(-5)`/`Lz4(<1 KiB)` on 512 KiB local frames + `Codec::None` on fixed-width/For/Delta/BitpackBool/Rle/Dict + AVX2 SBBF, no map shredding / no FSST / no trigrams / no `.stpk`; `L1Compact` = full cascade + ALP + sorted Dict + FSST (non-WideRow) + ShreddedMap + `Zstd(3)` on all non-inline mini-block encodings and shredded sub-columns + Structural JSON/FastCDC + BLAKE3-128 `.stpk` `Zstd(3)` 512 KiB frames + `TS02` trigram + boundary-pruned edge table + opt-in streaming Zstd dict via `new_with_dict` for `>= 2 MiB` packs.)*

---

## 7. Controlled `unsafe` SIMD, Miri & Untrusted-Input Bounds

- `crates/statam/Cargo.toml` duplicates all workspace lints explicitly with `unsafe_code = "deny"` (cannot inherit `workspace = true` since root has `forbid`); ONLY `src/simd/{bitpack,filter,bloom,trigram}.rs` carry `#[allow(unsafe_code)]`, with `// SAFETY:` on every `unsafe` block and `#[cfg(not(miri))]` + runtime CPU detection dispatching to the 100% safe scalar fallback.
- Miri tests isolate pure-Rust modules (all SIMD dispatch + scalar kernels, codecs with `None`/`Lz4`, container/header/postscript/corrupt-input parsers) from `zstd` C FFI via `#[cfg(not(miri))]` so `cargo miri test` runs cleanly with zero errors.
- **Hard Untrusted-Input Bounds (checked BEFORE allocating `Vec::with_capacity`):**
  - `MAX_ROWS_PER_FILE = u32::MAX as u64`
  - `MAX_HOT_TAIL_BYTES = 16 MiB` (writer targets `<= 128 KiB` default tail fetch with `120 KiB` soft / `1 MiB` hard Hot Tail budget)
  - `MAX_COLUMNS = 1,024`, `MAX_NESTING_DEPTH = 8`, `MAX_ROW_GROUPS = 262,144`, `MAX_TOTAL_BLOCKS = 4,194,304`
  - `MAX_BLOCK_RAW_BYTES = 64 MiB`, `MAX_FRAME_RAW_BYTES = 16 MiB`, `MAX_DECOMPRESS_RATIO = 1,024× + 16,384 B`
  - `MAX_SHREDDED_KEYS = 64`, `MAX_REF_CHAIN_DEPTH = 15`, `MAX_CHUNKS_PER_ROW = 65,535` (`u16::MAX`)
  - `MAX_ROW_PAYLOAD_BYTES = 64 MiB`, `MAX_BATCH_DECODED_BYTES = 256 MiB` (defeats dedup/tail-sharing amplification bombs!)
  - `MAX_DICT_BYTES = 32 MiB`, `MAX_BLOOM_BYTES = 32 MiB`
  - Every table allocation MUST check `count.checked_mul(entry_size) <= Some(remaining_bytes)` BEFORE allocating!
  - Workspace deps added to root `[workspace.dependencies]`: `blake3 = "1.8.7"`, `memchr = "2.8.3"` (both already present in `Cargo.lock`).

---

## 8. Milestones

- **Stage 1 (Done):** `prior-art.md` + `spec-v0.md` (cold-reviewed and finalized).
- **Stage 2 (Done — All 5 Go/No-Go Gates Passed; see `stage2-benchmark-report.md`):**
  - **M2.1 (Done):** Crate scaffold, error/bounds, header/postscript/format framing, SIMD kernels (`bitpack`, `filter`, `bloom`, `trigram`), and all 16-type narrow/structured codecs (`Const`, `Rle`, `ForBitpack`, `DeltaBitpack`, `Alp`, `Dict`, `Fsst`, `BitpackBool`, `Plain`, `List`, `ShreddedMap`, `Events`, `Links`).
  - **M2.2 (Done):** Payload chunker (Structural JSON + FastCDC + strict `>= 256 B` invariant), `.stpk` pack writer/reader + adaptive `TS02` trigram + boundary-pruned `head_63 + tail_63` edge table, `StatamWriter` (`L0Fast` & `L1Compact`, multi-threaded) + `StatamReader` (1-GET tail, pruning, late materialization, `Dict`/`Rle`/`Const` pass-through, `map_keys`, exact substring search, `take_rows`, multi-threaded), and single-rewrite `gdpr_rewrite_day` with 0-byte proof.
  - **M2.3 (Done):** `fuzz/fuzz_targets/statam_file.rs` + seed corpus + replay tests, Miri verification, ASan libFuzzer campaign, `SimulatedS3Reader` (exact 1 MiB `ReadCache`), 4-workload Cetra + 6-workload public external benchmarks (`W1`–`W4`, `ClickBench`, `Row-Filter`, `Random-Access`, `JSONBench`, `WildChat-1M`, `Hermes-FC`) against Parquet/Vortex/Lance/Strata on `precision-5770-vm`, and 10 optimization + 9 cold-review passes until all 5 Go/No-Go gates passed.
- **Stage 3 (Gated on User Sign-Off):** `cetra-storage` integration (`StatamSource`, `StatamFileWriter`, `catalog.db`, compaction, GDPR, verify, dropping Arrow/Parquet storage reads).
