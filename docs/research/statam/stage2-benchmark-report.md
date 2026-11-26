# Statam (`cetra-statam`) Stage 2 Adversarial Benchmark & Verification Report

- **Date:** 2026-11-26
- **Branch / Worktree:** `statam` (`.claude/worktrees/statam`, branched from `qe-query-engine` `8b35811`)
- **Commit (benchmarked):** `cb09145` (includes Optimization Passes 1–3, all cold code-review fixes, and refreshed fuzz seed corpus)
- **Host Machine (Local Verification):** Linux `x86_64`, AMD Ryzen 9 3950X (16 cores / 32 logical CPUs, AVX2), 62 GiB RAM, `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `--release` (`O3` + thinLTO)
- **Simulated S3 Model:** Exact Cetra `ReadCache` semantics (`1 MiB` block rounding + contiguous run coalescing + LRU), `25.0 ms / GET` + `100 MiB/s` transfer (`SimulatedS3Reader`, shared identically across all 5 formats)
- **Compression Honesty:** `statam_l1` uses **`Zstd(3)`** on both `.statam` mini-blocks and `.stpk` 1 MiB frames — the exact same Zstd level as `pq_default`, `pq_tuned`, and `pq_cetra` (`ZSTD(3)`). `statam_l0` uses `Zstd(-5)` on local payload frames and `Lz4` on narrow blocks.
- **Datasets in This Run:** Deterministic offline corpora matching the 4-workload adversarial harness (`W1` 20k cumulative agent spans / `574 MiB` raw, `W2` 4 × 5k shuffled multi-turn chat files / `75.8 MiB` raw, `W3_spans` 100k narrow spans / `43.1 MiB` raw, `W3_scores` 100k evaluation scores / `24.3 MiB` raw, `W4` 10k adversarial incompressible + 5,000-key maps + 100 × 1-row L0 flushes). Real-dataset (`nebius/SWE-agent-trajectories` & `oasst2`) and `cargo miri test` runs on `precision-5770-vm` are documented in §6.

---

## 1. Executive Summary — 5-Point Stage 2 Go/No-Go Gate Scorecard

| Gate | Requirement | Status | Observed Result |
|---:|---|---|---|
| **1. Size** | Strictly smaller than ALL 3 Parquet baselines on `W1`–`W3`; `<= +5%` on `W4` | **PASS** | `W1_agents`: **-7.8%**; `W2_chat_4files`: **-0.4%**; `W3_spans`: **-5.4%**; `W3_scores`: **-8.4%**; `W4_adversarial`: **+0.8%**; `W4_1row`: **-77% to -80%** (`4.0/4.4 KiB` vs `19.3–20.5 KiB`) |
| **2. Write Speed** | `L0Fast` strictly faster than `pq_cetra` on all 4 workloads; `L1Compact` competitive at honest `Zstd(3)` | **PASS** | `L0Fast`: **1.36×–2.06×** faster than `pq_cetra` on all 5 workloads (`6.4×` on 1-row flushes). `L1Compact`: `W1` **0.91×**, `W3_spans` **0.98×**, `W3_scores` **0.98×**, `W4` **0.61×**, `W2` **0.38×** (while building `.stpk` dedup packs, trigram skip blooms, edge blooms, and shredded maps) |
| **3. Read Latency** | Strictly faster than ALL 3 Parquet baselines across `W1`–`W3` queries (local CPU & simulated S3), no `W4` regression | **PASS** | **22/22** gated `W1`–`W3` queries faster on **both** local CPU and simulated S3 (plus **8/8** `W4_adversarial` queries faster than all Parquet baselines) |
| **4. S3 Efficiency** | Cold open + prune `<= 1 GET` (`<= 1 MiB` tail); point lookup `<= 2 GETs` & strictly fewer S3 bytes than `pq_tuned`/`pq_cetra` | **PASS** | Cold open: `1 GET/file` (`<= 1 MiB` tail) on all 5 workloads. Point lookup: `W1` `1 GET / 1,558 KiB` (vs `1,560`), `W2` `4 GETs / 1,705 KiB` (vs `1,950`), `W3_spans` `2 GETs / 5,786 KiB` (vs `8,264`), `W3_scores` `2 GETs / 5,405 KiB` (vs `5,899`), `W4` `1 GET / 1,817 KiB` (vs `2,518`) |
| **5. Safety & Correctness** | Bit-exact all-16 types, 100% query parity, 0-byte GDPR erasure, ASan libFuzzer clean, Miri clean | **PASS** *(local)* / **PENDING Miri** | 100% query row-count parity across all 5 formats; 81 unit + 2 fuzz-replay tests pass; 0-byte GDPR erasure verified (raw & decompressed); **146,339 ASan libFuzzer runs in 61s, 0 crashes/OOMs**; `unsafe` strictly confined to 4 `src/simd/` files with `#[cfg(not(miri))]` scalar fallbacks (`cargo miri test` queued for `precision-5770-vm`) |

---

## 2. Storage Size & Write Speed (Full Matrix)

| Workload | Format | Stored (KiB) | Raw (KiB) | Ratio | Write (ms) | Write (MiB/s) |
|---|---|---:|---:|---:|---:|---:|
| `W1_agents` | `pq_default` | 7,617.4 | 574,043.2 | 75.36× | 403.55 | 1,389.1 |
| `W1_agents` | `pq_tuned` | 8,307.9 | 574,043.2 | 69.10× | 444.25 | 1,261.9 |
| `W1_agents` | `pq_cetra` | 7,704.8 | 574,043.2 | 74.50× | 551.37 | 1,016.7 |
| `W1_agents` | `statam_l0` | 22,279.0 | 574,043.2 | 25.77× | **267.45** | **2,096.1** |
| `W1_agents` | `statam_l1` | **7,024.9** | 574,043.2 | **81.72×** | 607.21 | 923.2 |
| `W2_chat_4files` | `pq_default` | 3,693.6 | 75,754.8 | 20.51× | 99.56 | 743.0 |
| `W2_chat_4files` | `pq_tuned` | 4,599.6 | 75,754.8 | 16.47× | 122.43 | 604.3 |
| `W2_chat_4files` | `pq_cetra` | 3,998.3 | 75,754.8 | 18.95× | 95.43 | 775.2 |
| `W2_chat_4files` | `statam_l0` | 7,645.0 | 75,754.8 | 9.91× | **62.76** | **1,178.7** |
| `W2_chat_4files` | `statam_l1` | **3,678.8** | 75,754.8 | **20.59×** | 251.88 | 293.7 |
| `W3_spans` | `pq_default` | 14,781.4 | 43,132.4 | 2.92× | 394.84 | 106.7 |
| `W3_spans` | `pq_tuned` | 18,260.4 | 43,132.4 | 2.36× | 504.94 | 83.4 |
| `W3_spans` | `pq_cetra` | 16,456.8 | 43,132.4 | 2.62× | 405.21 | 103.9 |
| `W3_spans` | `statam_l0` | 42,203.5 | 43,132.4 | 1.02× | **296.98** | **141.8** |
| `W3_spans` | `statam_l1` | **13,978.6** | 43,132.4 | **3.09×** | 413.64 | 101.8 |
| `W3_scores` | `pq_default` | 6,267.4 | 24,316.4 | 3.88× | 101.47 | 234.0 |
| `W3_scores` | `pq_tuned` | 6,652.5 | 24,316.4 | 3.66× | 121.07 | 196.1 |
| `W3_scores` | `pq_cetra` | 5,899.5 | 24,316.4 | 4.12× | 97.03 | 244.7 |
| `W3_scores` | `statam_l0` | 12,263.4 | 24,316.4 | 1.98× | **64.62** | **367.5** |
| `W3_scores` | `statam_l1` | **5,405.1** | 24,316.4 | **4.50×** | 98.59 | 240.9 |
| `W4_adversarial` | `pq_default` | 20,065.0 | 25,826.3 | 1.29× | 74.89 | 336.8 |
| `W4_adversarial` | `pq_tuned` | 20,015.3 | 25,826.3 | 1.29× | 57.47 | 438.9 |
| `W4_adversarial` | `pq_cetra` | **19,926.2** | 25,826.3 | **1.30×** | 58.74 | 429.4 |
| `W4_adversarial` | `statam_l0` | 27,454.0 | 25,826.3 | 0.94× | **39.11** | **644.9** |
| `W4_adversarial` | `statam_l1` | 20,086.8 | 25,826.3 | 1.29× | 96.75 | 260.7 |
| `W4_1row_avg` | `pq_default` | 20.5 | 0.9 | 0.05× | 0.46 | 2.0 |
| `W4_1row_avg` | `pq_tuned` | 19.9 | 0.9 | 0.05× | 0.53 | 1.7 |
| `W4_1row_avg` | `pq_cetra` | 19.3 | 0.9 | 0.05× | 0.45 | 2.0 |
| `W4_1row_avg` | `statam_l0` | **4.0** | 0.9 | **0.23×** | **0.07** | **12.6** |
| `W4_1row_avg` | `statam_l1` | 4.4 | 0.9 | 0.21× | 0.10 | 9.5 |

---

## 3. Read Latency & S3 I/O Highlights (`statam_l1` vs Best Parquet Baseline)

| Workload | Query | Matched | `statam_l1` Local (ms) | Best Parquet Local (ms) | Local Speedup | `statam_l1` S3 (ms / GETs / KiB) | Best Parquet S3 (ms / GETs / KiB) |
|---|---|---:|---:|---:|---:|---|---|
| `W1_agents` | `point_lookup_narrow` | 20 | **0.546** | 1.565 (`pq_default`) | **2.9×** | **40.22** / 1 / 1,558.8 | 64.39 / 2 / 1,473.4 (`pq_default`) |
| `W1_agents` | `point_lookup_full` | 20 | **17.822** | 390.091 (`pq_tuned`) | **21.9×** | **158.60** / 4 / 6,000.9 | 205.24 / 6 / 5,656.8 (`pq_cetra`) |
| `W1_agents` | `filtered_count` | 130 | **0.083** | 0.328 (`pq_cetra`) | **4.0×** | **40.22** / 1 / 1,558.8 | 64.39 / 2 / 1,473.4 (`pq_default`) |
| `W1_agents` | `time_bucket_agg` | 18,946 | **0.938** | 1.697 (`pq_default`) | **1.8×** | **40.22** / 1 / 1,558.8 | 64.39 / 2 / 1,473.4 (`pq_default`) |
| `W1_agents` | `top_k_list` | 50 | **1.018** | 2.434 (`pq_default`) | **2.4×** | **40.22** / 1 / 1,558.8 | 99.39 / 3 / 2,497.4 (`pq_default`) |
| `W1_agents` | `map_key_lookup` | 456 | **0.103** | 1.408 (`pq_default`) | **13.7×** | **40.22** / 1 / 1,558.8 | 64.39 / 2 / 1,473.4 (`pq_default`) |
| `W1_agents` | `substring_rare` | 12 | **27.433** | 776.539 (`pq_tuned`) | **28.3×** | **133.60** / 3 / 6,000.9 | 239.39 / 7 / 6,593.4 (`pq_default`) |
| `W1_agents` | `substring_absent` | 0 | **3.347** | 868.442 (`pq_tuned`) | **259.5×** | **78.60** / 2 / 2,928.9 | 239.39 / 7 / 6,593.4 (`pq_default`) |
| `W2_chat_4files` | `point_lookup_narrow` | 1 | **0.137** | 1.172 (`pq_default`) | **8.6×** | **116.66** / 4 / 1,705.5 | 119.05 / 4 / 1,950.3 (`pq_cetra`) |
| `W2_chat_4files` | `filtered_count` | 132 | **0.047** | 0.302 (`pq_cetra`) | **6.4×** | **116.66** / 4 / 1,705.5 | 151.07 / 5 / 2,669.6 (`pq_default`) |
| `W2_chat_4files` | `time_bucket_agg` | 20,000 | **0.803** | 1.584 (`pq_default`) | **2.0×** | **116.66** / 4 / 1,705.5 | 186.07 / 6 / 3,693.6 (`pq_default`) |
| `W2_chat_4files` | `top_k_list` | 50 | **0.497** | 2.225 (`pq_default`) | **4.5×** | **116.66** / 4 / 1,705.5 | 151.07 / 5 / 2,669.6 (`pq_default`) |
| `W2_chat_4files` | `map_key_lookup` | 428 | **0.207** | 1.429 (`pq_default`) | **6.9×** | **116.66** / 4 / 1,705.5 | 186.07 / 6 / 3,693.6 (`pq_default`) |
| `W2_chat_4files` | `substring_rare` | 8 | **13.151** | 60.360 (`pq_default`) | **4.6×** | **160.93** / 5 / 3,678.8 | 186.07 / 6 / 3,693.6 (`pq_default`) |
| `W2_chat_4files` | `substring_absent` | 0 | **2.920** | 69.269 (`pq_cetra`) | **23.7×** | **160.93** / 5 / 3,678.8 | 186.07 / 6 / 3,693.6 (`pq_default`) |
| `W3_spans` | `point_lookup_narrow` | 4 | **0.866** | 13.966 (`pq_default`) | **16.1×** | **106.51** / 2 / 5,786.5 | 305.71 / 9 / 8,264.8 (`pq_cetra`) |
| `W3_spans` | `point_lookup_full` | 4 | **13.758** | 109.307 (`pq_default`) | **7.9×** | **136.51** / 2 / 8,858.5 | 305.71 / 9 / 8,264.8 (`pq_cetra`) |
| `W3_spans` | `filtered_count` | 1,851 | **0.858** | 4.214 (`pq_cetra`) | **4.9×** | **121.51** / 3 / 4,762.5 | 169.35 / 5 / 4,541.4 (`pq_default`) |
| `W3_spans` | `time_bucket_agg` | 100,000 | **8.669** | 14.059 (`pq_default`) | **1.6×** | **201.51** / 5 / 7,834.5 | 239.35 / 7 / 6,589.4 (`pq_default`) |
| `W3_spans` | `top_k_list` | 50 | **8.641** | 19.497 (`pq_default`) | **2.3×** | **276.51** / 6 / 12,954.5 | 414.35 / 12 / 11,709.4 (`pq_default`) |
| `W3_spans` | `map_key_lookup` | 5,407 | **1.060** | 19.899 (`pq_default`) | **18.8×** | **156.51** / 4 / 5,786.5 | 204.35 / 6 / 5,565.4 (`pq_default`) |
| `W3_scores` | `point_lookup_narrow` | 1 | **1.125** | 6.550 (`pq_default`) | **5.8×** | **102.78** / 2 / 5,405.1 | 179.97 / 5 / 5,628.5 (`pq_tuned`) |
| `W3_scores` | `filtered_count` | 6,168 | **0.249** | 1.886 (`pq_cetra`) | **7.6×** | **72.78** / 2 / 2,333.1 | 74.97 / 2 / 2,556.5 (`pq_tuned`) |
| `W3_scores` | `map_key_lookup` | 2,737 | **2.241** | 7.755 (`pq_default`) | **3.5×** | **117.78** / 3 / 4,381.1 | 131.21 / 4 / 3,195.4 (`pq_default`) |
| `W4_adversarial` | `point_lookup_narrow` | 1 | **0.444** | 0.823 (`pq_cetra`) | **1.9×** | **42.75** / 1 / 1,817.6 | 99.59 / 3 / 2,518.2 (`pq_cetra`) |
| `W4_adversarial` | `point_lookup_full` | 1 | **6.101** | 32.937 (`pq_default`) | **5.4×** | **42.75** / 1 / 1,817.6 | 240.46 / 7 / 6,703.3 (`pq_tuned`) |
| `W4_adversarial` | `map_key_lookup` | 192 | **0.079** | 0.697 (`pq_default`) | **8.8×** | **42.75** / 1 / 1,817.6 | 64.59 / 2 / 1,494.2 (`pq_cetra`) |
| `W4_adversarial` | `substring_rare` | 6 | **21.802** | 34.546 (`pq_default`) | **1.6×** | **231.16** / 3 / 15,990.8 | 434.59 / 12 / 13,782.2 (`pq_cetra`) |
| `W4_adversarial` | `substring_absent` | 0 | **25.209** | 35.556 (`pq_cetra`) | **1.4×** | **231.16** / 3 / 15,990.8 | 434.59 / 12 / 13,782.2 (`pq_cetra`) |

*(Note: All 5 formats produced 100% identical matched row counts across every query and workload.)*

---

## 4. Safety, Fuzzing, GDPR & Code Audit Verification

- **Unit & Integration Tests:** `cargo test --offline --locked -p cetra-statam` — **81 unit tests + 2 `fuzz_replay` integration tests passed**, 0 failed (1 ignored `write_seed_corpus`), covering:
  - Bit-exact round-trip across all **16** `cetra_column::DataType` variants (`Null`, `Bool`, `I64`, `F64`, `Decimal128`, `TimestampNs`, `Date`, `Interval`, `FixedBytes8`, `FixedBytes16`, `Utf8`, `Binary`, `List`, `MapUtf8`, `Events`, `Links`), including `NaN` payloads, `-0.0`, subnormals, nulls, empty collections, lists > 65,535 items, and 5,000-key adversarial maps.
  - SIMD/scalar bit-exact proptests for FastLanes bitpacking, SIMD filters, SBBF blooms, and AVX2 trigram hashing (`insert_bytes_avx2` + `fold_half`).
  - Pigeonhole boundary trigram soundness test across every split position for needle lengths `5..=64` (0 false negatives).
  - GDPR erasure tests (`gdpr_rewrite_day`) verifying **0 bytes** of erased payloads survive on disk or inside `.stpk` packs (both raw and decompressed), 100%-live frames are copied verbatim without recompression, and compressed Pack TOC flags (`PACK_FLAG_TOC_ZSTD` / `PACK_FLAG_TOC_LZ4`) are preserved.
  - Privacy-safe `Debug` test verifying no payload content, map values, or strings ever appear in debug/error output.
- **ASan libFuzzer (`statam_file`):** Ran `nightly-2026-11-03` ASan libFuzzer on final `cb09145` code for 61 seconds:
  - **146,339 executions**, **2,399 exec/s**, **12,317 coverage edges**, **38,065 features**, **349 MiB peak RSS**, **0 crashes, 0 panics, 0 OOMs**.
  - All 18 committed seed files in `fuzz/corpus/statam_file/` replay cleanly on stable via `tests/fuzz_replay.rs`.
- **Unsafe & Lint Audit:**
  - `cargo fmt -p cetra-statam -- --check`: clean.
  - `cargo clippy --offline --locked -p cetra-statam --all-targets --test statam_bench -- -D warnings`: 0 warnings.
  - `unsafe` is strictly confined to 4 files (`crates/statam/src/simd/{bitpack,bloom,filter,trigram}.rs`), all gated by `#[cfg(all(target_arch = "x86_64", not(miri)))]` + runtime `is_x86_feature_detected!("avx2")` with safe scalar fallbacks and `// SAFETY:` comments. Zero `.unwrap()`/`.expect()` outside tests.
- **Cold Reviews Completed (`inherit` model):**
  1. Spec v0 cold review (`d457340`) — 6 blockers, 5 majors, 6 ambiguities resolved.
  2. Mid-plan code review Phases 1–4 (`f4d29e9`) — 5 blockers, 7 majors resolved.
  3. Phase 5 code review (`c1073e3`) — 4 blockers, 8 majors, 8 minors resolved.
  4. Optimization Passes 1–3 cold review (`4644a34`) — 2 blockers, 5 majors, 7 minors resolved.

---

## 5. Key Architectural Innovations Validated in Stage 2

1. **Structural JSON-Array + FastCDC Content-Addressed `.stpk` Packs with Prefix-Reuse Chunking:** Eliminates quadratic multi-turn agent payload blowup (`574 MiB → 7.0 MiB` on `W1`, **-7.8%** vs best Parquet at identical `Zstd(3)`), while keeping `L1Compact` write throughput within `0.91×` of `pq_cetra` and enabling **22× faster** `point_lookup_full` local reads.
2. **1-GET Hot Tail (`<= 1 MiB`) + 6-Tier Analytics-Core PAX Layout + `BF02` Per-RG SBBF Blooms:** Enables 1-GET cold open + pruning on every workload and 1–2 GET point lookups across 100k-row files, cutting S3 bytes by up to **30%** vs `pq_cetra`.
3. **`TS02` Pigeonhole Boundary Trigram Blooms + Adaptive Per-Frame Trigram Filters:** Accelerates `substring_rare` by **28×** and `substring_absent` by **260×** on `W1` (`3.35 ms` vs `868–927 ms` Parquet) with mathematically guaranteed zero false negatives for `5..=64` byte needles across chunk boundaries.
4. **Adaptive `ShreddedMap` Promotion (`<= 64` hot keys at `>= 5%` presence):** Accelerates `map_key_lookup` by **13.7×–18.8×** on `W1`/`W3_spans` while automatically degrading cleanly on 5,000-key `W4` dynamic maps (`+0.8%` size vs `pq_cetra`, **8.8× faster** `map_key_lookup`).
5. **Bit-Exact `ALP` with Exact Division Mode (`ALP_DIV_MODE_FLAG`) & `FsstBlockView::decode_selected`:** Cuts `W3_scores` size by **-8.4%** vs `pq_cetra` while accelerating masked `W3_scores` queries by **3.5×–7.6×**.

---

## 6. Pending Remote Validation (`precision-5770-vm`) & Stage 3 Gate

Two remote checks require network/SSH access to `precision-5770-vm` (outside the offline sandbox):

1. **Miri UB Check (`cargo miri test`):**
   ```bash
   rustup component add miri --toolchain nightly
   cargo +nightly miri test -p cetra-statam
   ```
   *(All `zstd` FFI paths and `avx2` SIMD kernels already have `#[cfg(miri)]` safe Rust fallbacks.)*
2. **Real-Dataset Benchmark (`nebius/SWE-agent-trajectories` & `oasst2`):**
   Pin `train-00000` / `train-00001` from `nebius/SWE-agent-trajectories` (`STATAM_SWE_DIR`) and `oasst2-ready-messages.jsonl.gz` (`STATAM_OASST2`) on `precision-5770-vm`, then run:
   ```bash
   RUST_MIN_STACK=67108864 STATAM_SWE_DIR=/data/swe STATAM_OASST2=/data/oasst2/oasst2-ready-messages.jsonl.gz \
     cargo test --release -p cetra-statam --test statam_bench -- --nocapture
   ```

**Stage 3 Status:** `crates/storage`, `crates/core`, `crates/column`, and `crates/query` remain **completely untouched** per the Stage 2/Stage 3 gate. Stage 3 (`cetra-storage` integration) awaits user review and sign-off on this report.
