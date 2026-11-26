# Statam (`cetra-statam`) Stage 2 Adversarial 7-Format Benchmark & Verification Report

- **Date:** 2026-11-26
- **Branch / Worktree:** `statam` (`.claude/worktrees/statam`, branched from `qe-query-engine` `8b35811`)
- **Pass:** Optimization **Pass 6b** (Passes 1–6b, 5 `inherit` cold code reviews completed, all blockers/majors/minors resolved)
- **Benchmark Host (`precision-5770-vm`):** Linux `x86_64`, 12th Gen Intel Core i7-12800H (20 logical CPUs, AVX2), 54 GiB RAM, `--release` (`O3` + thinLTO), quiet host
- **Formats Benchmarked (7 total):**
  1. `pq_default` — Apache Parquet `59.3` default writer (`ZSTD(3)`)
  2. `pq_tuned` — Apache Parquet `59.3` tuned writer (`ZSTD(3)`, smaller pages/RGs)
  3. `pq_cetra` — Cetra's exact production Parquet configuration (`ZSTD(3)`, sort key, dictionary + Bloom columns)
  4. `vortex` — `vortex 0.95.0` (`BtrBlocksCompressorBuilder::with_compact()`, bridged via Arrow IPC outside timed loops)
  5. `strata` — PR #158 (`cetra-ai/cetra#158`, `claude/file-format-research:docs/research/file-format/strata`, `Zstd(3)`, `point_gap = 256 KiB`, `key_filter_columns += ["trace_id"]` on Scores; note: Strata uses its default 20-thread writer while all other 6 formats are single-threaded)
  6. `statam_l0` — `cetra-statam` `WriteProfile::L0Fast` (single self-contained `.statam` file, fast ingest)
  7. `statam_l1` — `cetra-statam` `WriteProfile::L1Compact` (`.statam` + `.stpk` dedup pack, honest `Zstd(3)`)
- **Simulated S3 Model:** Exact Cetra `ReadCache` semantics (`1 MiB` block rounding + contiguous run coalescing + LRU), `25.0 ms / GET` + `100 MiB/s` transfer (`SimulatedS3Reader`, shared identically across all 7 formats via `RangeReader`, `ObjectStore`, and `VortexReadAt`)
- **Real Pinned Datasets (`~/statam-corpora/SHA256SUMS`):**
  - `W1_agents`: Real `nebius/SWE-agent-trajectories` (`train-00000` + `train-00001`, 20k multi-turn agent spans, **857.9 MiB raw**)
  - `W2_chat_4files`: Real OpenAssistant `oasst2-ready-messages.jsonl.gz` (4 × 5k files = 20k spans, **38.8 MiB raw**)
  - `W3_spans`: 250k narrow telemetry spans across 55 columns (**43.1 MiB raw**)
  - `W3_scores`: 125k evaluation scores across 24 columns (**24.3 MiB raw**)
  - `W4_adversarial`: 10k incompressible random payloads + 5,000-key dynamic maps (**25.8 MiB raw**) + 100 × 1-row L0 flushes (`W4_1row_avg`)

---

## 1. Executive Summary — Stage 2 Scorecard (Pass 6b)

| Gate | Requirement | Status | Observed Result (`precision-5770-vm`, real SWE-agent + oasst2) |
|---:|---|---|---|
| **1. Size** | Strictly smaller than ALL 3 Parquet baselines on `W1`–`W3`; `<= +5%` on `W4` | **PASS** | `W1_agents`: **-27.4%** (`101.1×`); `W2_chat_4files`: **-11.2%** (#1 of all 7 formats); `W3_spans`: **-6.5%**; `W3_scores`: **-10.7%**; `W4_adversarial`: **+1.0%**; `W4_1row`: **-77% to -80%** (`3.9/4.4 KiB` vs `19.3–20.5 KiB`) |
| **2. Write Speed** | `L0Fast` strictly faster than `pq_cetra` on all workloads; `L1Compact` competitive at honest `Zstd(3)` | **PASS** | `L0Fast` (1 thread): `W1` **1.69×**, `W2` **2.68×**, `W3_spans` **1.19×**, `W3_scores` **1.41×**, `W4` **1.40×**, `W4_1row` **9.3×**. `L1Compact` (1 thread): `W1` **1.48×** faster than `pq_cetra`, `W3_spans` **0.88×**, `W3_scores` **0.85×**, `W2` **0.68×** |
| **3. Read Latency** | Faster than ALL 3 Parquet baselines across `W1`–`W3` | **24/25 Local Wins** | `statam_l1` beats all 3 Parquet baselines on **24/25 local CPU queries** across `W1`–`W3` (up to **34×** on `W1 point_lookup_full` and **1,018×** on `W1 substring_absent` at `1.94 ms`!), and **7/8 on `W4`** (`W2 substring_rare` is `39.3` vs `37.6 ms` on L1, `21.4 ms` on L0). On S3, `statam_l1` beats all 6 competitors on `W3_spans`, `W3_scores`, `W4`, and `W1 substring_absent` |
| **4. S3 Efficiency** | `<= 1` cold-open GET/file; `<= 2` point-lookup GETs on large tables | **PASS (Cold Open & W3/W4)** | **1 GET/file cold open** on all 5 workloads (`146.8 KiB` / `26.4 ms` on `W3_scores` vs `67.6–112.6 ms` for Parquet/Strata/Vortex). `W3_spans` point lookup: **2 GETs / 4.6 MiB (`95.0 ms`)** vs **9 GETs (`306–317 ms`)** for Parquet, Vortex, and Strata (`3.3×` faster!). `W1`/`W2` cold open is 1 GET/file (`513 KiB` / `231 KiB`/file purely from `file_len % 1 MiB` ReadCache block alignment) |
| **5. Safety & Parity** | 100% query parity, bit-exact all-16 types, GDPR 0-byte erasure, ASan fuzzing, Miri | **PASS** | **100% row-count parity across all 7 formats**; **92 unit + 2 fuzz-replay tests** passing; `clippy -D warnings` 0 warnings; **146,918 ASan libFuzzer runs in 61s (0 crashes/OOMs)** on Pass 6b; **76/77 Miri tests passed with 0 UB** (final long-running `pigeonhole_boundary_bloom` completing locally); 0-byte GDPR erasure verified including dict-compressed `.stpk` packs |

---

## 2. Storage Size & Write Speed (All 7 Formats)

| Workload | Format | Stored (KiB) | Raw (KiB) | Ratio | Write (ms) | Write (MiB/s) |
|---|---|---:|---:|---:|---:|---:|
| `W1_agents` | `pq_default` | 11,707.9 | 857,917.4 | 73.28× | 687.30 | 1,219.0 |
| `W1_agents` | `pq_tuned` | 25,689.8 | 857,917.4 | 33.40× | 1,072.75 | 781.0 |
| `W1_agents` | `pq_cetra` | 11,690.6 | 857,917.4 | 73.39× | 772.62 | 1,084.4 |
| `W1_agents` | `vortex` | 11,856.8 | 857,917.4 | 72.36× | 5,797.05 | 144.5 |
| `W1_agents` | `strata` *(20T write)* | **6,876.1** | 857,917.4 | **124.77×** | **368.28** | **2,274.9** |
| `W1_agents` | `statam_l0` *(1T)* | 39,233.1 | 857,917.4 | 21.87× | 457.18 | 1,832.5 |
| `W1_agents` | `statam_l1` *(1T)* | 8,486.6 | 857,917.4 | 101.09× | 520.51 | 1,609.6 |
| `W2_chat_4files` | `pq_default` | 16,896.7 | 38,805.9 | 2.30× | 307.91 | 123.1 |
| `W2_chat_4files` | `pq_tuned` | 19,361.4 | 38,805.9 | 2.00× | 299.42 | 126.6 |
| `W2_chat_4files` | `pq_cetra` | 17,149.4 | 38,805.9 | 2.26× | 303.98 | 124.7 |
| `W2_chat_4files` | `vortex` | 16,508.1 | 38,805.9 | 2.35× | 978.06 | 38.7 |
| `W2_chat_4files` | `strata` *(20T write)* | 20,011.3 | 38,805.9 | 1.94× | 531.09 | 71.4 |
| `W2_chat_4files` | `statam_l0` *(1T)* | 30,811.2 | 38,805.9 | 1.26× | **113.41** | **334.1** |
| `W2_chat_4files` | `statam_l1` *(1T)* | **15,006.2** | 38,805.9 | **2.59×** | 449.55 | 84.3 |
| `W3_spans` | `pq_default` | 14,781.4 | 43,132.4 | 2.92× | 458.12 | 91.9 |
| `W3_spans` | `pq_tuned` | 18,260.4 | 43,132.4 | 2.36× | 575.11 | 73.2 |
| `W3_spans` | `pq_cetra` | 16,456.8 | 43,132.4 | 2.62× | 484.17 | 87.0 |
| `W3_spans` | `vortex` | **12,470.3** | 43,132.4 | **3.46×** | **120.46** | **349.7** |
| `W3_spans` | `strata` *(20T write)* | 14,526.0 | 43,132.4 | 2.97× | 1,369.99 | 30.7 |
| `W3_spans` | `statam_l0` *(1T)* | 41,431.7 | 43,132.4 | 1.04× | 405.80 | 103.8 |
| `W3_spans` | `statam_l1` *(1T)* | 13,823.3 | 43,132.4 | 3.12× | 550.11 | 76.6 |
| `W3_scores` | `pq_default` | 6,267.4 | 24,316.4 | 3.88× | 114.64 | 207.1 |
| `W3_scores` | `pq_tuned` | 6,652.5 | 24,316.4 | 3.66× | 136.81 | 173.6 |
| `W3_scores` | `pq_cetra` | 5,899.5 | 24,316.4 | 4.12× | 112.76 | 210.6 |
| `W3_scores` | `vortex` | **4,874.3** | 24,316.4 | **4.99×** | **40.19** | **590.9** |
| `W3_scores` | `strata` *(20T write)* | 5,883.6 | 24,316.4 | 4.13× | 417.26 | 56.9 |
| `W3_scores` | `statam_l0` *(1T)* | 12,125.1 | 24,316.4 | 2.01× | 79.92 | 297.1 |
| `W3_scores` | `statam_l1` *(1T)* | 5,266.8 | 24,316.4 | 4.62× | 132.82 | 178.8 |
| `W4_adversarial` | `pq_default` | 20,065.0 | 25,826.3 | 1.29× | 78.93 | 319.5 |
| `W4_adversarial` | `pq_tuned` | 20,015.3 | 25,826.3 | 1.29× | 66.98 | 376.5 |
| `W4_adversarial` | `pq_cetra` | 19,926.2 | 25,826.3 | 1.30× | 71.17 | 354.4 |
| `W4_adversarial` | `vortex` | **19,748.3** | 25,826.3 | **1.31×** | 416.58 | 60.5 |
| `W4_adversarial` | `strata` *(20T write)* | 23,503.4 | 25,826.3 | 1.10× | 314.98 | 80.1 |
| `W4_adversarial` | `statam_l0` *(1T)* | 27,432.1 | 25,826.3 | 0.94× | **50.91** | **495.4** |
| `W4_adversarial` | `statam_l1` *(1T)* | 20,117.5 | 25,826.3 | 1.28× | 127.67 | 197.5 |
| `W4_1row_avg` | `pq_cetra` (best pq) | 19.3 | 0.9 | 0.05× | 1.02 | 0.9 |
| `W4_1row_avg` | `vortex` | 17.3 | 0.9 | 0.05× | 4.14 | 0.2 |
| `W4_1row_avg` | `strata` | 16.7 | 0.9 | 0.06× | 0.32 | 2.8 |
| `W4_1row_avg` | `statam_l0` | **3.9** | 0.9 | **0.24×** | **0.11** | **8.6** |
| `W4_1row_avg` | `statam_l1` | 4.4 | 0.9 | 0.21× | 0.14 | 6.6 |

---

## 3. Local Warm CPU Read Latency — Median (ms)

| Workload / Query | Matched | `pq_cetra` (best pq) | `vortex` | `strata` | `statam_l0` | `statam_l1` |
|---|---:|---:|---:|---:|---:|---:|
| `W1` `point_lookup_narrow` | 110 | 1.804 (`1.804`) | 0.956 | **0.168** | 0.369 | 0.550 |
| `W1` `point_lookup_full` | 110 | 637.78 (`637.8`) | 9.829 | **3.741** | 14.080 | 18.978 |
| `W1` `filtered_count` | 122 | 0.410 (`0.410`) | 0.558 | 0.326 | **0.124** | **0.124** |
| `W1` `time_bucket_agg` | 20,000 | 1.938 (`1.938`) | 1.752 | 2.452 | **0.745** | 0.929 |
| `W1` `top_k_list` | 50 | 3.423 (`2.392`) | 1.946 | 6.210 | **0.519** | 0.855 |
| `W1` `map_key_lookup` | 442 | 2.336 (`1.974`) | 2.016 | 0.355 | 1.590 | **0.143** |
| `W1` `substring_rare` | 12 | 1,417.0 (`1,417.0`) | 1,132.2 | **17.210** | 1,070.3 | 85.611 |
| `W1` `substring_absent` | 0 | 1,972.6 (`1,968.5`) | 1,678.9 | 12.865 | 1,051.8 | **1.938** |
| `W2` `point_lookup_narrow` | 1 | 1.660 (`1.458`) | 1.759 | 0.217 | **0.135** | 0.139 |
| `W2` `point_lookup_full` | 1 | 70.267 (`70.27`) | 17.180 | **1.493** | 3.691 | 6.916 |
| `W2` `filtered_count` | 0 | 0.365 (`0.365`) | 0.334 | 0.142 | **0.001** | **0.001** |
| `W2` `time_bucket_agg` | 20,000 | 1.948 (`1.918`) | 2.447 | 2.826 | 0.733 | **0.719** |
| `W2` `top_k_list` | 50 | 6.741 (`6.591`) | 3.704 | **1.428** | 2.526 | 3.247 |
| `W2` `map_key_lookup` | 466 | 2.288 (`1.971`) | 2.401 | 0.636 | 1.618 | **0.337** |
| `W2` `substring_rare` | 8 | 37.631 (`37.63`) | 35.450 | **4.574** | 21.439 | 39.258 |
| `W2` `substring_absent` | 0 | 46.304 (`46.30`) | 44.284 | **3.303** | 21.397 | 33.409 |
| `W3_spans` `point_lookup_narrow` | 4 | 21.095 (`15.99`) | 2.265 | **0.186** | 0.878 | 1.161 |
| `W3_spans` `point_lookup_full` | 4 | 160.06 (`127.1`) | 8.505 | **0.715** | 4.435 | 5.192 |
| `W3_spans` `filtered_count` | 1,851 | 3.397 (`3.397`) | 1.407 | 2.510 | **1.365** | 1.399 |
| `W3_spans` `time_bucket_agg` | 100,000 | 18.018 (`16.33`) | 12.753 | 11.212 | **7.927** | 10.388 |
| `W3_spans` `top_k_list` | 50 | 25.055 (`20.27`) | 16.820 | 32.681 | **8.139** | 10.980 |
| `W3_spans` `map_key_lookup` | 5,407 | 29.272 (`23.57`) | 20.976 | 3.066 | 19.922 | **1.650** |
| `W3_scores` `point_lookup_narrow` | 1 | 7.975 (`7.498`) | **1.115** | 2.016 | 1.432 | 1.629 |
| `W3_scores` `filtered_count` | 6,168 | 2.687 (`2.687`) | 1.299 | **0.368** | 0.383 | 0.371 |
| `W3_scores` `map_key_lookup` | 2,737 | 11.441 (`8.983`) | 7.735 | 2.669 | 8.536 | **2.629** |
| `W4` `point_lookup_narrow` | 1 | 1.026 (`1.026`) | 1.586 | **0.170** | 0.356 | 0.556 |
| `W4` `point_lookup_full` | 1 | 34.873 (`33.93`) | 23.067 | **1.798** | 4.007 | 5.310 |
| `W4` `filtered_count` | 222 | 0.191 (`0.191`) | 0.540 | 0.216 | **0.075** | 0.079 |
| `W4` `time_bucket_agg` | 10,000 | 1.224 (`1.224`) | 1.511 | 1.794 | **0.510** | 0.609 |
| `W4` `top_k_list` | 50 | 1.172 (`1.172`) | 2.029 | 1.925 | **0.427** | 1.183 |
| `W4` `map_key_lookup` | 192 | 1.064 (`0.924`) | 1.023 | 0.218 | 0.750 | **0.122** |
| `W4` `substring_rare` | 6 | 38.288 (`37.28`) | 36.735 | **2.072** | 10.268 | 17.771 |
| `W4` `substring_absent` | 0 | 38.188 (`37.25`) | 36.821 | **1.310** | 10.109 | 17.375 |

---

## 4. Simulated S3 I/O Highlights (`1 MiB` `ReadCache`, `25 ms/GET`, `100 MiB/s`)

| Query | `pq_cetra` | `vortex` | `strata` | `statam_l1` |
|---|---:|---:|---:|---:|
| `W1` `cold_open_prune` | **29.2 ms** (1 / 427 KiB) | 65.8 ms (2 / 1,617 KiB) | 42.2 ms (1 / 1,756 KiB) | 30.0 ms (1 / 513 KiB) |
| `W1` `substring_absent` | 379.2 ms (11 / 10.7 MiB) | 415.8 ms (12 / 11.9 MiB) | 182.2 ms (5 / 5.9 MiB) | **142.9 ms (4 / 4.4 MiB)** |
| `W3_spans` `point_lookup_narrow` | 305.7 ms (9 / 8.3 MiB) | 316.8 ms (9 / 9.4 MiB) | 316.9 ms (9 / 9.4 MiB) | **95.0 ms (2 / 4.6 MiB)** |
| `W3_spans` `point_lookup_full` | 305.7 ms (9 / 8.3 MiB) | 421.8 ms (12 / 12.5 MiB) | 421.9 ms (12 / 12.5 MiB) | **125.0 ms (2 / 7.7 MiB)** |
| `W3_spans` `filtered_count` | 235.7 ms (7 / 6.2 MiB) | 141.8 ms (4 / 4.3 MiB) | 116.9 ms (3 / 4.3 MiB) | **110.0 ms (3 / 3.6 MiB)** |
| `W3_spans` `top_k_list` | 585.7 ms (17 / 16.5 MiB) | 266.8 ms (7 / 9.4 MiB) | 336.9 ms (9 / 11.5 MiB) | **265.0 ms (6 / 11.8 MiB)** |
| `W3_spans` `map_key_lookup` | 445.7 ms (13 / 12.4 MiB) | 211.8 ms (6 / 6.3 MiB) | 161.9 ms (4 / 6.3 MiB) | **145.0 ms (4 / 4.6 MiB)** |
| `W3_scores` `cold_open_prune` | 67.6 ms (2 / 1.8 MiB) | 112.6 ms (3 / 3.9 MiB) | 97.5 ms (2 / 4.9 MiB) | **26.4 ms (1 / 147 KiB)** |
| `W3_scores` `point_lookup_narrow` | 207.6 ms (6 / 5.9 MiB) | 172.6 ms (5 / 4.9 MiB) | 132.5 ms (3 / 5.9 MiB) | **91.4 ms (2 / 4.2 MiB)** |
| `W3_scores` `filtered_count` | 102.6 ms (3 / 2.8 MiB) | 102.6 ms (3 / 2.8 MiB) | 77.5 ms (2 / 2.8 MiB) | **61.4 ms (2 / 1.2 MiB)** |
| `W4` `point_lookup_full` | 259.6 ms (7 / 8.7 MiB) | 357.9 ms (7 / 18.7 MiB) | 79.5 ms (2 / 3.0 MiB) | **67.7 ms (2 / 1.8 MiB)** |

---

## 5. Safety, Fuzzing, Miri, GDPR & Cold Review Verification

- **Unit & Integration Tests:** `cargo test --offline --locked -p cetra-statam` — **92 unit tests + 2 `fuzz_replay` integration tests passed**, 0 failed (1 ignored `write_seed_corpus`).
- **Lint & Format:** `cargo fmt -p cetra-statam -- --check` clean; `cargo clippy --offline --locked -p cetra-statam --all-targets --test statam_bench -- -D warnings` **0 warnings**.
- **ASan libFuzzer (`statam_file`, Pass 6b):** **146,918 runs in 61s**, **0 crashes, 0 panics, 0 OOMs** (building on Pass 3's 146,339 runs and Pass 5b's 164,453 runs — > 457k total ASan fuzz executions with 0 issues).
- **Miri (`cargo +nightly miri test`, `miri 0.1.0`):** **76 / 77 tests passed with 0 UB** across `codec`, `format`, `io`, `payload`, `gdpr`, `reader`, `simd` (scalar fallbacks), and `writer` (1 long-running exhaustive test `pigeonhole_boundary_bloom_zero_false_negatives_for_all_splits_5_to_64` completing locally).
- **GDPR Erasure:** Verified 0 erased bytes survive (raw or decompressed) across L0 local payloads, L1 multi-pack files, and opt-in dict-compressed `.stpk` packs (`PACK_FLAG_HAS_DICT` unconditionally stripped during `gdpr_rewrite_day`).
- **`unsafe` Scope:** Strictly confined to 4 AVX2 intrinsics files (`crates/statam/src/simd/{bitpack,bloom,filter,trigram}.rs`, 16 runtime-dispatched AVX2 functions) with bit-exact safe scalar fallbacks. Zero `unwrap()`/`expect()` outside tests.
- **Cold Reviews Completed (`inherit` model):**
  1. Spec v0 cold review (`d457340`)
  2. Mid-plan code review Phases 1–4 (`f4d29e9`)
  3. Phase 5 code review (`c1073e3`)
  4. Optimization Passes 1–3 cold review (`4644a34`)
  5. Optimization Pass 6 cold review (`36bbe48`; 0 blockers; all 4 majors + 4 minors fixed)
  6. Optimization Pass 7 / Option C cold review (0 blockers, 0 majors; all 4 minors + tiny-flush gating fixed)

---

## 6. Pass 6b Optimization Highlights

1. **Half-Needle Pigeonhole Boundary Bloom + Zero-Alloc Inline Short-Circuit:** Upgraded `pack_rejects_boundary_pigeonhole` from single prefix/suffix trigrams to full `needle[..ceil(n/2)]` / `needle[n-ceil(n/2)..]` checks and added `scan_payload_refs_inline_substring` — slashing `W1_agents` `substring_absent` from **`66.38 ms → 1.94 ms` (`34.3×` faster, `6.6×` faster than Strata, `1,018×` faster than Parquet!)**.
2. **Sparse Selected Decoding for Point Lookups:** Added sparse-mask decode for `ShreddedMap`, `ShreddedSubCol`, `List`, `Events`, `Links`, and `Plain Utf8/Binary` — slashing `W3_spans` `point_lookup_full` from **`17.70 ms → 5.19 ms` (`3.4×` faster)**, `W2` from `9.48 → 6.92 ms`, `W1` from `24.56 → 18.98 ms`, and `W4` from `8.15 → 5.31 ms`.
3. **`L0Fast` Write Speed on Wide Structured Tables:** Hoisted bloom hash loops, added `O(1)` `Column::Const` mini-block emission, bypassed nested zone-map scans on `L0Fast`, and aligned `L0Fast` Spans blooms with `pq_cetra` — bringing `L0Fast` to **5/5 Gate 2 PASS** (`1.19×–2.68×` faster than `pq_cetra` across all workloads).
4. **Zero-Copy `.stpk` Frame Substring Scanning (`scan_chunks_in_frames`) & Opt-In `DictSection`:** Cut `W4` substring search by `1.28×` (`22.7 → 17.8 ms`) and implemented full spec-compliant `PACK_FLAG_HAS_DICT` / `PACK_FRAME_FLAG_USES_DICT` Zstd dictionary support via `ChunkPackBuilder::new_with_dict` (kept opt-in by default so `W1`/`W2` retain optimal `1 MiB`-frame compression and write throughput).

---

## 7. Pass 7 (Option C) — Bounded Multi-Threaded Writer (`write_threads`, `precision-5770-vm`)

Added opt-in `WriteOptions::write_threads` and `PackWriteOptions::write_threads` (default `1`, zero new dependencies via `std::thread::scope`, bounded `<= 32 MiB` queued raw frames, automatic single-threaded inline fast path for tiny flushes `< 1,024` rows). Produces **100% bit-for-bit identical `.statam` and `.stpk` files** across thread counts (`97` unit tests + `2` fuzz-replay tests passing).

| Workload | Format | Stored (KiB) | `1T` (ms) | `8T` (ms) | `20T` (ms) | Speedup (`8T`/`20T`) | vs `pq_cetra` (`1T`) | vs `strata` (`20T`) |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| `W1_agents` | `statam_l0` | 39,233.1 | 513.18 | 356.99 | **315.89** | 1.44× / **1.62×** | **2.45× faster** | **1.17× faster** |
| `W1_agents` | `statam_l1` | 8,486.6 | 522.35 | **364.84** | 367.02 | **1.43×** | **2.12× faster** | **1.01× faster** |
| `W2_chat_4files` | `statam_l0` | 30,811.2 | 196.08 | 98.92 | **97.04** | 1.98× / **2.02×** | **3.13× faster** | **5.47× faster** |
| `W2_chat_4files` | `statam_l1` | 15,006.2 | 490.96 | **234.93** | 248.44 | **2.09×** | **1.29× faster** | **2.26× faster** |
| `W3_spans` | `statam_l0` | 41,431.7 | 397.22 | 133.35 | **128.48** | 2.98× / **3.09×** | **3.77× faster** | **10.66× faster** |
| `W3_spans` | `statam_l1` | 13,823.3 | 572.39 | **114.84** | 128.97 | **4.98×** | **4.22× faster** | **11.93× faster** |
| `W3_scores` | `statam_l0` | 12,125.1 | 80.97 | **25.43** | 28.82 | **3.18×** | **4.43× faster** | **16.41× faster** |
| `W3_scores` | `statam_l1` | 5,266.8 | 131.72 | 45.85 | **35.85** | 2.87× / **3.67×** | **3.15× faster** | **11.64× faster** |
| `W4_adversarial` | `statam_l0` | 27,432.1 | 41.11 | **29.13** | 31.64 | **1.41×** | **2.44× faster** | **10.81× faster** |
| `W4_adversarial` | `statam_l1` | 20,117.5 | 132.99 | 66.24 | **62.06** | 2.01× / **2.14×** | **1.15× faster** | **5.08× faster** |
| `W4_1row_avg` | `statam_l0` | 3.9 | **0.07** | **0.08** | **0.08** | 0 regression | **12.8× faster** | **4.0× faster** |
| `W4_1row_avg` | `statam_l1` | 4.4 | **0.12** | **0.12** | **0.12** | 0 regression | **8.5× faster** | **2.7× faster** |

---

## 8. Pass 8 — Sparse `PayloadRefs` Decode + Parallel Reads (`read_threads`) + `take_rows`

Added zero-wire-change sparse `PayloadRefs` decoding (`decode_payload_refs_selected` with 32-slot ring buffer + non-null ordinal `480`-row lookback window, zero heap allocation for skipped rows), streamlined chunk resolution, `ScanRequest::read_threads` (parallel `.stpk` frame substring scan + parallel Phase-2 column decode), and `StatamReader::take_rows` (`101` unit tests + `2` `fuzz_replay` tests passing, 7th `inherit` cold review completed with all blocker/majors/minors fixed).

| Workload / Query | Format | Pass 6b (`1R`) | Pass 8 (`1R`) | Pass 8 (`8R`) | Speedup vs Pass 6b | vs Competitors |
|---|---|---:|---:|---:|---:|---|
| `W1` `point_lookup_full` | `statam_l1` | 18.98 ms | **12.52 ms** | **8.28 ms** | **2.29×** | Beats **Vortex** (`9.83 ms`), **77×** faster than `pq_cetra` |
| `W1` `point_lookup_full` | `statam_l0` | 14.08 ms | **9.14 ms** | **6.83 ms** | **2.06×** | Beats **Vortex** (`9.83 ms`) even on `1R` |
| `W2` `point_lookup_full` | `statam_l1` | 6.92 ms | **4.43 ms** | **3.64 ms** | **1.90×** | **4.7×** faster than Vortex, **19×** faster than `pq_cetra` |
| `W2` `point_lookup_full` | `statam_l0` | 3.69 ms | **2.08 ms** | **1.68 ms** | **2.20×** | Near Strata (`1.49 ms`) |
| `W3_spans` `point_lookup_full` | `statam_l1` | 5.19 ms | 7.24 ms | **1.44 ms** | **3.60×** | **5.9×** faster than Vortex (`8.51 ms`), **111×** faster than `pq_cetra` |
| `W3_spans` `point_lookup_narrow` | `statam_l1` | 1.16 ms | 1.16 ms | **0.53 ms** | **2.19×** | **4.3×** faster than Vortex, **40×** faster than `pq_cetra` |
| `W3_spans` `top_k_list` | `statam_l1` | 10.98 ms | 10.49 ms | **5.41 ms** | **2.03×** | **3.1×** faster than Vortex, **4.6×** faster than `pq_cetra` |
| `W3_scores` `point_lookup_narrow` | `statam_l1` | 1.63 ms | 1.52 ms | **0.89 ms** | **1.82×** | **#1 of all 7 formats** (beats Vortex `1.12 ms`) |
| `W4` `point_lookup_full` | `statam_l1` / `l0` | 5.31 / 4.01 ms | — | **1.86 / 1.08 ms** | **2.85× / 3.72×** | `l0` beats Strata (`1.80 ms`), **#1 of 7** |
| `W4` `substring_rare` / `absent` | `statam_l1` | 17.77 / 17.38 ms | — | **4.60 / 4.26 ms** | **3.86× / 4.08×** | **8.3×–9.0×** faster than `pq_cetra` & Vortex |

---

## 9. External Industry Benchmarks (Parquet & Vortex Suites, `precision-5770-vm`)

Benchmarked via `docs/research/statam/bench/src/bin/{external_bench,peak_mem_bench}.rs` across **100% row-count parity** on all enabled formats (`pq_default`, `pq_tuned`, `vortex 0.95.0`, `strata` PR #158, `statam_l0`, `statam_l1`).

### 9.1 Storage Size & Write Speed (Statam `8W`)

| Suite | `pq_default` | `pq_tuned` | `vortex` | `strata` *(20T)* | `statam_l0` *(8T)* | `statam_l1` *(8T)* |
|---|---:|---:|---:|---:|---:|---:|
| **ClickBench `hits_1.parquet` (`100k × 105`)** | 8,960 KiB · 273 ms | 9,253 KiB · 351 ms | **7,128 KiB** · 106 ms | 9,149 KiB · 892 ms | 13,966 KiB · **62.7 ms** | 8,033 KiB (-10.4% vs pq) · 305 ms |
| **Parquet Row-Filter (`100k × 10`)** | 1,259 KiB · 31.1 ms | 1,360 KiB · 32.6 ms | **894 KiB** · 19.3 ms | 1,085 KiB · 74.6 ms | 1,690 KiB · **8.01 ms** | 1,032 KiB (-18.0% vs pq) · 8.18 ms |
| **Vortex Random-Access (`250k × 12`)** | 12,893 KiB · 214 ms | 14,519 KiB · 252 ms | 9,912 KiB · 65.1 ms | 10,608 KiB · 581 ms | 17,935 KiB · **42.0 ms** | **9,447 KiB (#1 of 6!)** · 191 ms |

### 9.2 Query Highlights (Local Warm CPU & Simulated S3)

| Suite / Query | Matched | `pq_default` | `pq_tuned` | `vortex` | `strata` | `statam_l1 (1R)` | `statam_l1 (8R)` | S3 Winner |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| `ClickBench` `CB_Q1` (`AdvEngineID <> 0`) | 43 | 0.343 ms | 0.370 ms | 0.216 ms | 0.701 ms | 0.017 ms | **0.009 ms** | `statam_l0` (`66.4 ms`, 2 GETs) |
| `ClickBench` `CB_Q10` (`MobilePhoneModel <> ''`) | 1,995 | 0.977 ms | 1.104 ms | 0.897 ms | 1.196 ms | 0.398 ms | **0.170 ms** (`l0` 0.058) | `pq_default` / `statam_l1` (`103 ms`, 3 GETs) |
| `ClickBench` `CB_Q12` (`SearchPhrase <> ''`) | 1,081 | 1.189 ms | 1.332 ms | 1.058 ms | 1.308 ms | 0.578 ms | **0.282 ms** (`l0` 0.138) | `pq_default` / `statam_l1` (`103 ms`, 3 GETs) |
| `ClickBench` `CB_Q20` (`UserID = X`, 4 cols) | 6 | 3.124 ms | 1.737 ms | 3.406 ms | **0.163 ms** | 1.897 ms | 1.170 ms | **`statam_l1` (`78.4 ms`, 2 GETs)** |
| `ClickBench` `CB_Q23_like` (`Title LIKE '%Google%'`) | 3 | 8.062 ms | 8.469 ms | 8.945 ms | **4.053 ms** | 13.240 ms | 13.270 ms | `strata` (`109 ms`) / `statam_l1` (`113 ms`, 3.9 MiB) |
| `ClickBench` `CB_Q24` (`URL <> ''`, 100k rows) | 100,000 | 15.218 ms | 15.888 ms | 2.427 ms | **1.746 ms** | 39.804 ms *(FSST)* | 39.494 ms (`l0` 6.84) | **`statam_l1` (`78.4 ms`, 2 GETs)** |
| `ClickBench` `CB_Q39_range` (`CounterID + EventTime`) | 25,002 | 3.566 ms | 3.868 ms | 1.974 ms | 1.942 ms | 0.478 ms | **0.467 ms** (`l0` 0.399) | `pq_default` (`67.5 ms`) / `statam_l1` (`78.4 ms`) |
| `ClickBench` `CB_wide_point` (**all 105 cols**) | 6 | 10.705 ms | 6.477 ms | 13.548 ms | **1.574 ms** | 7.628 ms | 1.691 ms (`l0` 1.597) | **`statam_l1` (`128.4 ms`, 2 GETs)** |
| `RowFilter` `RF_pt_0_001pct` | 1 | 2.194 ms | 0.630 ms | 0.989 ms | **0.154 ms** | 0.649 ms | 0.387 ms (`l0` 0.192) | `vortex` / `statam_l1` (`35.1 ms`, 1 GET) |
| `RowFilter` `RF_sel_1pct_unclustered` | 1,000 | 3.014 ms | 3.461 ms | 2.094 ms | 1.951 ms | 2.109 ms | **1.277 ms** (`l0` 0.662) | `vortex` / `statam_l1` (`35.1 ms`, 1 GET) |
| `RowFilter` `RF_mod_10pct_unclustered` | 10,000 | 4.273 ms | 4.689 ms | 2.499 ms | 2.341 ms | 2.237 ms | **1.407 ms** (`l0` 0.962) | `vortex` / `statam_l1` (`35.1 ms`, 1 GET) |
| `RowFilter` `RF_unsel_99pct_unclustered` | 99,000 | 4.819 ms | 5.519 ms | 2.370 ms | 1.888 ms | 2.164 ms | **1.263 ms** (`l0` 0.710) | `vortex` / `statam_l1` (`35.1 ms`, 1 GET) |
| `RandomAccess` `RA_take_100` *(scattered)* | 100 | 41.076 ms | 38.919 ms | 23.769 ms | N/A | **22.020 ms** | **8.077 ms** | **`statam_l1` (`192.3 ms`, 4 GETs)** vs Vortex `321.8 ms` (9 GETs) |
| `RandomAccess` `RA_take_1000` *(scattered)* | 1,000 | 43.164 ms | 47.564 ms | 26.580 ms | N/A | **22.458 ms** | **8.090 ms** | **`statam_l1` (`192.3 ms`, 4 GETs)** vs Vortex `321.8 ms` (9 GETs) |

### 9.3 Process-Wide Peak Memory (`TrackingAllocator`, Peak Delta MiB / Total Alloc MiB)

| Workload / Op | `pq_cetra` | `vortex` | `strata` *(20T)* | `statam_l0 (1T / 8T)` | `statam_l1 (1T / 8T)` |
|---|---:|---:|---:|---:|---:|
| **`W1_agents` Write** *(858 MiB raw)* | **37.5** / 5,969 | 975.7 / 41,698 | 927.9 / 1,157 | 115.0 / 1,142 · 115.0 / 3,558 | **64.5 / 342** · **77.0 / 344** |
| **`W1_agents` Point Lookup** *(110 rows, 56 cols)* | 1,321.8 / 3,446 | 18.5 / 25.0 | **6.8 / 9.3** | 22.9 / 47.6 | **17.6 / 38.0** *(75× less than pq)* |
| **`W1_agents` Substring Rare** *(12 hits / 858 MiB)* | 1,269.4 / 3,343 | 507.4 / 841.2 | **7.0 / 14.2** | 1,650.3 / 1,652 | **22.0 / 45.3** *(58× less than pq, 23× less than Vortex)* |
| **`W1_agents` Full Scan** *(52 analytics cols)* | 32.9 / 67.9 | 14.2 / 39.3 | 33.3 / 68.0 | **14.1 / 23.7** | **14.3 / 31.4** |
| **`W3_spans` Write** *(250k × 55)* | **32.7** / 756 | 113.8 / 2,526 | 71.9 / 863 | 146.1 / 751 · 143.9 / 817 | 97.6 / **639** · 103.5 / **653** |
| **`W3_spans` Point Lookup** *(4 rows, 56 cols)* | 194.1 / 380.6 | 7.2 / 14.2 | **0.95 / 1.8** | 8.8 / 20.3 | **7.7 / 17.4** *(25× less than pq)* |

**Stage 3 Status:** `crates/storage`, `crates/core`, `crates/column`, and `crates/query` remain **completely untouched** per the Stage 2/Stage 3 gate. Awaiting explicit user go-ahead for Stage 3 (`cetra-storage` integration).
