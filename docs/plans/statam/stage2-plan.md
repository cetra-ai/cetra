# Statam Stage 2 Implementation Plan (`crates/statam`)

**Summary:** Implement `crates/statam` (`cetra-statam`) per finalized [spec-v0.md](../../research/statam/spec-v0.md), including controlled `unsafe` SIMD kernels (`bitpack`, `filter`, `bloom`, `trigram`), all 16 `cetra-column` type codecs + `ShreddedMap`, Structural JSON/FastCDC payload chunking + BLAKE3-128 per-`(project, day)` `.stpk` packs + exact substring search, `L0Fast`/`L1Compact` `StatamWriter` and `StatamReader`, single-rewrite `gdpr_rewrite_day`, `statam_file` libFuzzer target + replay tests, and the 4-workload adversarial benchmark suite against `pq_default`, `pq_tuned`, and `pq_cetra`.

**Prerequisites:**
- Branch `statam` in `.claude/worktrees/statam` (branched from `qe-query-engine` at `8b35811`).
- Never edit `main`, `.claude/worktrees/qe-query-engine`, `crates/storage`, `crates/column`, `crates/query`, or `crates/core`.

**Read first:**
- `docs/research/statam/spec-v0.md` (source of truth for every byte offset, struct size, encoding, bound, and invariant)
- `docs/research/statam/prior-art.md`
- `crates/column/src/{types.rs,column.rs,batch.rs,bitmap.rs,selection.rs,frame.rs}`
- `crates/storage/src/read_cache.rs` (lines 520–596, 732–800: exact 1 MiB `ReadCache` rounding and run coalescing)

**What it is, and what it is not:**
- **Is:** A complete, standalone, fuzzed, Miri-ready, benchmarked `crates/statam` crate proven against all 5 Stage 2 Go/No-Go gates.
- **Is not:** `cetra-storage` integration (`StatamSource`, `catalog.db`, compaction/deletion hooks) — that is Stage 3, strictly gated on Stage 2 passing.

**Decisions this plan relies on:**
| ID | Summary |
|---|---|
| D-ST-1 | PAX 16,384-row Row Groups (contiguous Core + Wide-Row) with 4,096-row Mini-Blocks; LZ4-compressible Hot Tail ≤ 1 MiB |
| D-ST-2 | BLAKE3-128 chunk hashes (in separate compaction-only table) + CRC-32C integrity checksums + raw-block LZ4 (`lz4_flex::block`) |
| D-ST-3 | 8-lane SIMD Split-Block Bloom Filter (SBBF) with deterministic `statam_bloom_hash` |
| D-ST-4 | Structural JSON Array Chunker + FastCDC fallback enforcing strict `>= 256 B` non-last chunk invariant |
| D-ST-5 | Opt-in trained Zstd dicts, unconditionally discarded on GDPR pack rewrite |
| D-ST-6 | Meta Nimble flat-map shredding (`>= 5%` presence, `<= 64` keys, RG0-seeded streaming) with `in_map` + `value_null` bitmaps |

**New Workspace Dependencies (already present in `Cargo.lock`):**
- `blake3 = "1.8.7"` — root `[workspace.dependencies]`, cryptographic 128-bit chunk content hashes.
- `memchr = "2.8.3"` — root `[workspace.dependencies]`, SIMD `memmem` substring search.
- Existing workspace deps reused: `cetra-column`, `crc32c`, `lz4_flex`, `zstd`, `bytes`, `thiserror`, `proptest` (dev), `tempfile` (dev), `parquet`/`arrow`/`cetra-core`/`serde_json`/`rand` (dev-only for Stage 2 benchmark baselines).

---

## Phase Dependency Graph

```text
Phase 1 (scaffold-format)
   ├──► Phase 2 (simd-kernels) ──► Phase 4 (narrow-codecs) ──┐
   └──► Phase 3 (chunker-and-pack) ──────────────────────────┼──► Phase 5 (writer-reader-gdpr)
                                                                     ├──► Phase 6 (fuzz-and-hardening)
                                                                     └──► Phase 7 (bench-suite)
```

---

## Phase 1: Crate Scaffold, Wire Format & Range I/O (`scaffold-format`)

- **Goal:** Create `crates/statam` with explicit lints, privacy-safe `StatamError`, all exact wire structs/headers/postscripts/bounds, block/frame framing, and `RangeReader` implementations including exact 1 MiB `SimulatedS3Reader`.
- **Edit:**
  - `Cargo.toml` (add `cetra-statam`, `blake3 = "1.8.7"`, `memchr = "2.8.3"` under `[workspace.dependencies]`)
  - `crates/statam/Cargo.toml` (duplicate workspace lints with `unsafe_code = "deny"`)
  - `crates/statam/src/lib.rs`
  - `crates/statam/src/error.rs`
  - `crates/statam/src/format.rs`
  - `crates/statam/src/io.rs`
- **Contracts & Exact Details:**
  - `StatamError` variants: `Corrupt { reason: &'static str }`, `UnsupportedVersion { found: u16, expected: u16 }`, `UnsupportedFeature { mask: u32 }`, `BoundsExceeded { reason: &'static str }`, `ChecksumMismatch { context: &'static str }`, `EncryptionRequired`, `SchemaMismatch { reason: &'static str }`, `MissingPack { pack_id: [u8; 16] }`, `Io(#[from] std::io::Error)`, `Column(#[from] cetra_column::ColumnError)`. Privacy-safe `Display`/`Debug`.
  - Exact wire structs matching `spec-v0.md` §3 & §5.2 byte-for-byte: `FileHeader` (32 B), `Postscript` (112 B), `PackHeader` (48 B), `PackPostscript` (112 B), `ColumnDirEntry` (56 B), `RowGroupEntry` (48 B), `BlockEntry` (64 B), `FrameEntry` (40 B), `ChunkLoc` (12 B), `LazyIndexEntry` (24 B), `TableKind`, `Codec::{None=0, Lz4=1, Zstd=2}`, `Encoding` (`0..=14`), `LocalityGroup::{Core=0, WideRow=1, ExternalPayload=2}`.
  - `encode_schema` / `decode_schema` supporting all 16 `cetra_column::DataType` variants + `ShreddedMapDir` + `pack_ids` + `sort_cols` + `table_kind`, calling `DataType::validate(MAX_NESTING_DEPTH)`.
  - `compress_guarded(raw, codec, zstd_level)` -> `(Codec, Vec<u8>)` (returns `(Codec::None, raw)` unless `compressed.len() + 16 < raw.len()`).
  - `decompress_checked(stored, codec, raw_len, max_raw)` -> enforces `raw_len <= max_raw` and `raw_len <= stored.len() * 256 + 4096` BEFORE allocating.
  - `write_framed_block` / `read_framed_block` (16 B `SB` header + CRC-32C); `write_framed_pack_frame` / `read_framed_pack_frame` (16 B `PF` header + CRC-32C).
  - `RangeReader` trait (`file_len`, `read_range`, `read_ranges`) + `SliceReader`, `FileReader`, `SimulatedS3Reader` (exact `BLOCK = 1 << 20` rounding, `runs(&missing)` coalescing, LRU block cache of configurable capacity, `IoCounters { gets, bytes_from_s3, bytes_requested, cache_hits, cache_misses, waves }`, simulated latency `gets * rtt_ms + bytes_from_s3 / bandwidth`).
- **Tests:**
  - `header_postscript_exact_sizes_and_roundtrip` (asserts 32/112/48/112/56/48/64/40/12/24 exact byte sizes)
  - `schema_roundtrip_all_16_data_types_and_shredded_maps`
  - `rejects_corrupt_crc_unknown_version_unknown_feature_encrypted_flag`
  - `rejects_zip_bomb_and_oversized_counts_without_oom`
  - `incompressibility_guard_falls_back_to_raw`
  - `simulated_s3_reader_exact_1mib_block_rounding_and_coalescing`
- **Done when:** `cargo test -p cetra-statam && cargo clippy -p cetra-statam --all-targets -- -D warnings`
- **Commit:** `feat(statam): add crate scaffold, binary wire format, and RangeReader I/O`

---

## Phase 2: Controlled `unsafe` SIMD Kernels (`simd-kernels`)

- **Goal:** Implement all 4 audited SIMD kernels with `// SAFETY:` comments, `#[cfg(not(miri))]` + runtime CPU dispatch, safe scalar fallbacks, and bit-for-bit differential tests.
- **Edit:**
  - `crates/statam/src/simd/mod.rs`
  - `crates/statam/src/simd/bitpack.rs`
  - `crates/statam/src/simd/filter.rs`
  - `crates/statam/src/simd/bloom.rs`
  - `crates/statam/src/simd/trigram.rs`
  - `crates/statam/src/lib.rs` (add `pub mod simd;`)
- **Contracts & Exact Details:**
  - `bitpack.rs`:
    - `pack_u32(values: &[u32], bit_width: u8, out: &mut Vec<u8>)` / `unpack_u32(src: &[u8], count: usize, bit_width: u8, out: &mut Vec<u32>) -> Result<(), StatamError>`: FastLanes-1024 interleaved layout for full 1024-value tiles (8-lane AVX2 `_mm256_srlv_epi32`/`_mm256_and_si256` + unaligned `_mm256_loadu_si256`/`_mm256_storeu_si256`), compact scalar tail for `count % 1024`.
    - `pack_u64` / `unpack_u64` (bit widths `0..=64`): FastLanes-1024 4-lane AVX2 for full tiles + compact scalar tail.
    - `delta_encode_i64` / `delta_decode_i64` (FastLanes `04261537` 8-tile parallel prefix sum for monotonic / zigzag deltas).
  - `filter.rs`:
    - `FilterOp::{Eq, Ne, Lt, Le, Gt, Ge}`, `filter_i64(values: &[i64], op: FilterOp, rhs: i64) -> Bitmap`, `filter_u32`, `filter_f64` (respecting IEEE `NaN` comparisons accurately), `filter_i64_between(values: &[i64], lo: i64, hi: i64) -> Bitmap`: AVX2 `_mm256_cmpgt_epi64`/`_mm256_cmpeq_epi64`/`_mm256_movemask_pd` producing packed `u64` words straight into `Bitmap::from_words`.
  - `bloom.rs`:
    - Deterministic `statam_bloom_hash(bytes: &[u8]) -> u64` (pure-Rust WyHash/SplitMix64 mixer with fixed golden constants).
    - `Sbbf`: `new_for_ndv(ndv: usize, fpp: f64)`, `insert_hash(&mut self, h: u64)`, `insert_bytes(&mut self, b: &[u8])`, `contains_hash(&self, h: u64) -> bool`, `contains_bytes(&self, b: &[u8]) -> bool`, `to_bytes() -> Vec<u8>` (`BF01` header + CRC-32C), `from_bytes(&[u8]) -> Result<Self, StatamError>`. AVX2 8-lane `_mm256_mullo_epi32` + `_mm256_srli_epi32(27)` + `_mm256_sllv_epi32` block probe/insert!
  - `trigram.rs`:
    - `ascii_lowercase_into(src: &[u8], dst: &mut Vec<u8>)`: AVX2 32-byte parallel `'A'..='Z' -> | 0x20` (non-ASCII untouched!).
    - `extract_ascii_trigram_hashes(bytes: &[u8], ascii_only_for_ilike: bool, out: &mut Vec<u32>)`: deterministic trigram hash.
    - `TrigramBloom`: `new(byte_len: usize)`, `insert_bytes(&mut self, bytes: &[u8])`, `may_contain_hashes(&self, hashes: &[u32]) -> bool`, `bit_density(&self) -> f64`.
- **Tests:**
  - `bitpack_u32_u64_simd_matches_scalar_all_widths_and_lengths` (`0..=32` / `0..=64`, lengths `[0, 1, 7, 15, 16, 31, 32, 63, 64, 1023, 1024, 1025, 2048, 4096]`)
  - `delta_i64_simd_matches_scalar_monotonic_and_zigzag`
  - `filter_i64_u32_f64_simd_matches_scalar_including_nan_and_inf`
  - `sbbf_simd_matches_scalar_zero_false_negatives_and_fpp_bound`
  - `trigram_ascii_lowercase_simd_matches_scalar_preserves_utf8`
  - `bitpack_rejects_truncated_input_and_invalid_bit_width`
- **Done when:** `cargo test -p cetra-statam simd && cargo clippy -p cetra-statam --all-targets -- -D warnings`
- **Commit:** `feat(statam): add audited AVX2 SIMD kernels with scalar fallbacks`

---

## Phase 3: Payload Chunker, `.stpk` Chunk Packs & Exact Substring Search (`chunker-and-pack`)

- **Goal:** Implement Structural JSON Array + FastCDC chunking (strict `>= 256 B` non-last invariant), BLAKE3-128 `.stpk` pack builder/reader with 1 MiB `ReadCache` alignment, tail-sharing ref pages, and exact substring search.
- **Edit:**
  - `crates/statam/src/payload/mod.rs`
  - `crates/statam/src/payload/chunker.rs`
  - `crates/statam/src/payload/pack.rs`
  - `crates/statam/src/payload/search.rs`
  - `crates/statam/src/lib.rs` (add `pub mod payload;`)
- **Contracts & Exact Details:**
  - `chunker.rs`: `chunk_payload<'a>(data: &'a [u8], out: &mut Vec<&'a [u8]>)`: Structural JSON array splitter + Gear-hash FastCDC (`MIN_CHUNK=256`, `CDC_MIN=512`, `CDC_AVG=2048`, `MAX_CHUNK=8192`). Enforces: (a) concatenation of output slices equals `data` byte-for-byte, (b) every non-last chunk has `len >= 256`, (c) handles malformed JSON/unclosed quotes cleanly without panic.
  - `pack.rs`: `ChunkPackBuilder` (BLAKE3-128 dedup, adaptive 64 KiB raw / 24 KiB compressed Zstd-3 frames, 1 MiB `ReadCache` boundary guard, adaptive trigram filter `<= 10%` compressed frame & omitted if `< 2560 B` or `> 50%` saturated, `head_63 + tail_63` `EdgeTable`, separate `ChunkHashTable`, LZ4-compressed `PackToc` + 112 B `PackPostscript`) and `ChunkPackReader` (`open`, `read_chunks`, `read_frame`, `read_edge_table`, `read_trigram_section`, `read_hash_table`, raw-frame copy iterator for GDPR).
  - `payload/mod.rs`: `PayloadRefRow::{Null, Inline(Vec<u8>), Chunks(Vec<(u16, u32)>)}`, `encode_payload_refs` / `decode_payload_refs` with tail-sharing (`back 1..=32`, `keep_prefix`, depth `<= 15`, delta-varint chunk IDs) + `MAX_CHUNKS_PER_ROW` / `MAX_ROW_PAYLOAD_BYTES` / `MAX_BATCH_DECODED_BYTES` enforcement.
  - `search.rs`: `search_pack_chunks(pack, needle, case_insensitive)` (ASCII-only trigram bloom skip + unique-chunk `memchr::memmem` + `EdgeTable` cross-chunk boundary check + `> 64 B` exact fallback + non-ASCII `ILIKE` Unicode fallback).
- **Tests:**
  - `chunker_json_array_reuses_prefix_and_enforces_256b_non_last_invariant`
  - `chunker_malformed_json_and_oversized_elements_roundtrip_exact`
  - `pack_builder_and_reader_roundtrip_1mib_boundary_guard_and_dedup`
  - `payload_refs_tail_sharing_collapses_multiturn_and_bounds_depth_15`
  - `substring_search_exact_interior_boundary_absent_long_and_unicode_ilike`
  - `payload_refs_rejects_amplification_bomb_and_cycle_depth`
- **Done when:** `cargo test -p cetra-statam payload && cargo clippy -p cetra-statam --all-targets -- -D warnings`
- **Commit:** `feat(statam): add structural JSON/FastCDC chunker, .stpk packs, and substring search`

---

## Phase 4: Narrow, Nested & Shredded-Map Codecs + Zone Maps (`narrow-codecs`)

- **Goal:** Implement all narrow, structured (`List`, `Events`, `Links`), and `ShreddedMap` encoders/decoders for all 16 `cetra_column::DataType`s, plus sound zone-map construction and pruning.
- **Edit:**
  - `crates/statam/src/codec/mod.rs`
  - `crates/statam/src/codec/numeric.rs`
  - `crates/statam/src/codec/alp.rs`
  - `crates/statam/src/codec/fsst.rs`
  - `crates/statam/src/codec/dict.rs`
  - `crates/statam/src/codec/nested.rs`
  - `crates/statam/src/codec/zonemap.rs`
  - `crates/statam/src/lib.rs` (add `pub mod codec;`)
- **Contracts & Exact Details:**
  - `alp.rs`: SIGMOD-2024 ALP (`exp: 0..=18`, `fac: 0..=exp`) via sample-based `(e, f)` selection + FastLanes `ForBitpack` + sparse `(u16, u64)` bit-exact exception list. Guaranteed `decoded.to_bits() == input.to_bits()` for every `f64` including `-0.0`, signaling/quiet `NaN` payloads, `±inf`, subnormals.
  - `fsst.rs`: VLDB-2020 FSST trainer (≤ 255 symbols of 1..=8 bytes, fast 2–4 iteration gain-based training on sample), encoder (`0xFF` escape), and fast decoder supporting both full-block decode and O(1) per-row decode. Falls back to `Plain` if ratio < 1.15×.
  - `dict.rs`: File-level `DictBuilder` (bounded `<= 4096` insertion-order for `L0Fast`, full sorted for `L1Compact`) + wire codec (`DT01` header + CRC-32C) + `Column::Dict` pass-through when `raw_bytes <= 256 KiB`.
  - `numeric.rs`: `ConstNull`, `Const` (inline ≤ 16 B & empty collections), `Rle` (emits `Column::Rle` with per-run validity), `ForBitpack` (`i64` & `Decimal128` `i128` base), `DeltaBitpack`, `BitpackBool`, `Plain` (incl. `Interval`, `FixedBytes8/16`, `Decimal128`, `Utf8`, `Binary`).
  - `nested.rs`: `ListEncoded` (recursive up to `MAX_NESTING_DEPTH = 8`, dict-encoded `List(Utf8)` leaf), `EventsStruct`, `LinksStruct`, and `ShreddedMap` (RG0-seeded `<= 64` hot keys `>= 5%` presence; promoted sub-columns store `in_map` + `value_null` bitmaps + typed encoding; rare & duplicate keys in key-dict residual; direct single-key projection `decode_shredded_key`!).
  - `zonemap.rs`: `compute_zone_map(&Column)` & `zone_map_may_match(&BlockEntry, &DataType, col_flags, &ZonePred) -> bool` with exact `NaN`/`-0.0`, raw-byte carry-increment `Utf8`/`Binary`, `Decimal128` 16 B, `Interval` (`MIN_MAX_VALID=0`), and `DICT_SORTED`-gated range pruning.
- **Tests:**
  - `alp_bit_exact_roundtrip_costs_neg_zero_nan_payloads_inf_subnormals`
  - `fsst_roundtrip_and_incompressible_fallback`
  - `all_16_data_types_roundtrip_l0_and_l1_with_nulls_empty_and_edge_cases`
  - `shredded_map_roundtrip_null_values_duplicate_keys_and_w4_5000_keys_cap`
  - `zonemap_pruning_soundness_nan_utf8_carry_overflow_and_unsorted_dict`
  - `rle_and_dict_passthrough_emit_encoded_cetra_columns`
- **Done when:** `cargo test -p cetra-statam codec && cargo clippy -p cetra-statam --all-targets -- -D warnings`
- **Commit:** `feat(statam): implement 16-type narrow/structured codecs, ALP, FSST, shredded maps, and zone maps`

---

## Phase 5: `StatamWriter`, `StatamReader` & Airtight GDPR Erasure (`writer-reader-gdpr`)

- **Goal:** Wire end-to-end `StatamWriter` (`L0Fast` & `L1Compact`), `StatamReader` (1-GET Hot Tail, pruning, late materialization, `map_keys`, substring search, `Dict`/`Rle`/`Const` pass-through), and single-rewrite `gdpr_rewrite_day`.
- **Edit:**
  - `crates/statam/src/writer.rs`
  - `crates/statam/src/reader.rs`
  - `crates/statam/src/gdpr.rs`
  - `crates/statam/src/lib.rs` (public exports)
- **Contracts & Exact Details:**
  - `writer.rs`:
    - `WriteProfile::{L0Fast, L1Compact}`, `WriteOptions`, `WriteStats`.
    - Streams batches into 16,384-row PAX Row Groups (each split into 4,096-row mini-blocks); contiguous `Core` + `WideRow` (+ `L0Payload` in `L0Fast`) layout; RG0 seeds `L1Compact` shredded map keys and sorted file-level dicts.
    - Builds SBBF blooms (`trace_id` in Hot Tail; `span_id`/`session_id`/`user_id`/`score_id` in Hot Tail while tail ≤ 768 KiB else Lazy Index section).
    - Builds narrow-column trigram index for `spans.name` in `L1Compact`.
    - Compresses `hot_tail_body` with `lz4_flex::block` (`HOT_TAIL_LZ4`) whenever smaller, writing 112 B `Postscript`.
  - `reader.rs`:
    - `StatamFileMeta::read_from(reader, tail_hint)` — 1-GET cold open!
    - `StatamReader::scan(&ScanRequest)`:
      1. File-level pruning (Hot Bloom + Lazy Bloom on demand + `time_range` + `id_lookups`).
      2. RG & mini-block pruning (zone maps + binary search on sort key for `Fixed16`/`TimestampNs` lookups; reverse RG iteration when `sort_desc_hint && limit.is_some()`).
      3. Two-phase late materialization: fetch & evaluate filter columns first (using SIMD `filter.rs` + dict-code pushdown + `map_keys` shredded sub-col + substring search); skip remaining projected columns completely on 0-hit mini-blocks!
      4. Query-aware I/O coalescing (`Point` vs `Scan`): for point lookups, fetch the target RG's `core_offset..wide_offset+wide_len` in 1 contiguous GET!
      5. Supports `projection = Some(vec![])` (0-col row-count batch), `Dict`/`Rle`/`Const` pass-through, and exact `Selection`.
  - `gdpr.rs`:
    - `gdpr_rewrite_day`: single-rewrite protocol (§5.4) — collects live chunk bitmap across all surviving files/rows, builds `new_pack.stpk` (byte-for-byte copy of 100%-live frames, decompress+purge mixed frames, drop 0%-live frames, unconditionally discard any old `DictSection`), rewrites affected/pack-referencing `.statam` files **once** with remapped IDs, deletes old packs, and reports `frames_copied_verbatim`, `frames_rewritten`, `frames_dropped`, `bytes_purged`.
- **Tests:**
  - `end_to_end_roundtrip_spans_traces_scores_l0_and_l1`
  - `cold_open_is_1_get_and_point_lookup_is_le_2_gets_on_simulated_s3`
  - `late_materialization_skips_wide_columns_on_empty_filter`
  - `shredded_map_key_query_reads_only_subcolumn_and_skips_residual`
  - `substring_search_e2e_inline_pack_boundary_and_spans_name`
  - `gdpr_rewrite_day_leaves_zero_erased_bytes_raw_and_decompressed_and_copies_live_frames`
- **Done when:** `cargo test -p cetra-statam && cargo clippy -p cetra-statam --all-targets -- -D warnings`
- **Commit:** `feat(statam): implement StatamWriter, StatamReader, and single-rewrite GDPR erasure`

---

## Phase 6: Fuzz Target, Seed Corpus & Miri Hardening (`fuzz-and-hardening`)

- **Goal:** Add committed `statam_file` libFuzzer target + seed corpus + stable replay test, run libFuzzer, and verify pure-Rust/SIMD test suite is Miri-clean.
- **Edit:**
  - `crates/statam/src/fuzz_entry.rs`
  - `crates/statam/src/lib.rs` (expose `#[cfg(any(test, fuzzing))] pub mod fuzz_entry;`)
  - `crates/statam/tests/fuzz_replay.rs`
  - `fuzz/Cargo.toml` (add `cetra-statam` dep + `statam_file` bin)
  - `fuzz/fuzz_targets/statam_file.rs`
  - `fuzz/corpus/statam_file/` (generated seed files)
  - `justfile` (add `statam_file` to `fuzz_targets` and `_fuzz-max-len` = `1048576`)
- **Tests & Verification:**
  - `fuzz_entry::fuzz_statam_file(&[u8])` exercises standalone `.statam` open/scan/prune, standalone `.stpk` open/read/search, and split-input paired `.statam` + `.stpk` scan + `gdpr_rewrite_day`.
  - Generate seed corpus covering `L0Fast`, `L1Compact`, `.stpk`, shredded maps, all 16 types, and truncated/corrupted variants.
  - Run `cargo +nightly-2026-11-03 fuzz run statam_file fuzz/corpus/statam_file -- -max_total_time=60 -rss_limit_mb=4096 -max_len=1048576` with **0 crashes/OOMs/panics**.
  - Stable replay test `crates/statam/tests/fuzz_replay.rs` replays every corpus file on stable Rust.
- **Done when:** `cargo test -p cetra-statam && cargo +nightly-2026-11-03 fuzz run ... (0 crashes)`
- **Commit:** `test(statam): add statam_file fuzz target, seed corpus, and replay tests`

---

## Phase 7: Adversarial 4-Workload Benchmark Suite & Gate Verification (`bench-suite`)

- **Goal:** Build the head-to-head benchmark binary comparing Statam (`L0Fast` & `L1Compact`) against `pq_default`, `pq_tuned`, and `pq_cetra` across `W1`–`W4` on local warm/cold AND simulated S3 (`25 ms/GET`, `100 MB/s`, exact 1 MiB `ReadCache`).
- **Edit:**
  - `crates/statam/Cargo.toml` (dev-dependencies: `parquet`, `arrow`, `cetra-core`, `serde_json`, `rand`)
  - `crates/statam/tests/statam_bench.rs` (`[[test]] name = "statam_bench" harness = false` or `test = false` matching `crates/storage/tests/trace_bench.rs`)
- **Workloads (`W1`–`W4`):**
  - `W1`: Real SWE-agent trajectories loader (`nebius/SWE-agent-trajectories` Parquet shards when provided via `--swe-dir`, plus deterministic realistic SWE-agent trajectory generator when offline).
  - `W2`: Multi-turn chat sessions across a day (cross-file shared session history & system prompts, loader for `oasst2` when provided + realistic fallback).
  - `W3`: High-throughput narrow spans (500k–1M rows) & scores (250k+ rows): filtered count, time-bucketed aggregates, group-by model, top-50 `start_time DESC`, `map['customer_id']`, `trace_detail` & `span_detail` point lookups.
  - `W4`: Adversarial / incompressible / high-cardinality: random high-entropy prompts, high-cardinality UUIDs, 5,000-distinct-key dynamic maps, tiny 1-row L0 flushes, mixed nulls, `NaN`/`±inf`, empty lists/maps.
- **Baselines:** Exact `pq_default`, `pq_tuned`, and `pq_cetra` settings from `docs/research/file-format/bench/src/main.rs` & `crates/storage/src/parquet.rs`, read through the **exact same `SimulatedS3Reader` (1 MiB `ReadCache`)** so comparisons are 100% apples-to-apples!
- **Done when:** `cargo test --release -p cetra-statam --test statam_bench -- --nocapture` runs cleanly and prints the full 5-point Go/No-Go gate table.
- **Commit:** `bench(statam): add 4-workload adversarial benchmark suite vs Parquet baselines`
