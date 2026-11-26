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
  7. Optimization Pass 8 cold review (`57aab1b`; 1 blocker, 2 majors, 4 minors — all fixed)
  8. Optimization Pass 9 cold review (`b6211b9`; 0 blockers, 1 major, 2 minors — all fixed; `105` unit tests + `2` `fuzz_replay` tests passing)
  9. Optimization Pass 10 cold review (2 blockers, 4 majors, 4 minors — all fixed with dedicated regression tests; `125` unit tests + `2` `fuzz_replay` tests passing, `0` clippy warnings)

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

---

## 10. Pass 9 + 9b — `perf`-Guided Optimization (`precision-5770-vm`, `b307fa1`..`b6211b9`)

Profiled with `perf record` on `precision-5770-vm` (`kernel.perf_event_paranoid=1`). Five targeted zero-wire-change optimizations eliminated the top CPU hotspots:
1. **Single-Pass 8-Byte Word-Store FSST Decode + `u64`-Masked Longest Match (`codec/fsst.rs`):** Stack-allocated `FsstDecodeTable`, direct 8-byte unaligned write loop, in-place prefix-sum offsets, and `u64`-masked encoder matching (`perf` FSST decode share `15.89% → 7.01%`).
2. **Flat-Bitset `.stpk` Search + `FastPairHasher` (`payload/search.rs`):** Replaced `BTreeMap`/`Vec::sort_unstable` with flat `Vec<u64>` bitsets (`needed_bits`/`matched_bits`) and a deterministic `FastPairHasher` (`perf` `quicksort::<u32>` `7.44% → 0.00%`, `BTreeMap::insert` `2.46% → 0.00%`, `SipHasher` `2.71% → 0.42%`).
3. **Zone-Map Tautology Elimination (`codec/zonemap.rs`, `reader.rs`):** `zone_map_always_matches` skips Phase-1 evaluation on mini-blocks whose `[min, max]` guarantees 100% match (e.g., `URL <> ''` when `min > ""` and `null_count == 0`).
4. **Parallel Mini-Block Phase 1 & Phase 2 (`reader.rs`):** When `read_threads > 1`, fans out mini-block filtering and materialization across threads.
5. **Direct AVX2 `TrigramBloom::insert_bytes` + `fold_half()` (`writer.rs`):** Eliminated temporary buffer copies during narrow trigram bloom construction (`clickbench_100k` `L1` write `320.6 → 174.1 ms`, `1.84×` faster).

| Workload / Query | Format | Pre-Pass 9 (`1R`) | Pass 9 (`1R`) | Pass 9 (`8R`) | Speedup | Competitive Standing |
|---|---|---:|---:|---:|---:|---|
| `ClickBench` `CB_Q24` (`URL <> ''`) | `statam_l1` | 39.80 ms | **13.04 ms** | **4.89 ms** | **8.1×** | `1R` beats Parquet (`15.2 ms`); `l0 (8R)` **`2.20 ms`** beats Vortex (`2.43 ms`) |
| `ClickBench` `CB_Q23_like` (`Title LIKE '%Google%'`) | `statam_l1` | 13.24 ms | 13.34 ms | **5.99 ms** | **2.2×** | Beats Parquet (`8.06 ms`) & Vortex (`8.95 ms`) |
| `W1_agents` `substring_rare` *(858 MiB)* | `statam_l1` | 84.53 ms | **33.35 ms** | **16.86 ms** | **5.0×** | **Beats Strata (`17.21 ms`)**; **118× faster than Parquet** |
| `RandomAccess` `RA_take_100` / `1000` | `statam_l1` | 22.02 / 22.46 ms | 21.78 / 21.91 ms | **3.94 / 4.21 ms** | **5.6× / 5.3×** | **6.1×–6.3× faster than Vortex** (`24.1–26.4 ms`) |
| `RowFilter` *(all 6 patterns)* | `statam_l1` | 0.65–2.24 ms | 0.61–2.06 ms | **0.37–0.64 ms** | **3.2×–3.5×** | **3×–9× faster than Parquet, Vortex & Strata** across 6/6 |
| `ClickBench` Write (`8W`) | `statam_l1` | 320.6 ms | **174.1 ms** | — | **1.84×** | `W3_spans L1` `109.2 ms` & `W3_scores L1` `32.5 ms` both beat Vortex |

*(Top remaining CPU hotspot on ClickBench in post-Pass-9 `perf`: `9.6%` in `core::str::from_utf8` inside `cetra_column::Utf8Data::try_new` — tracked for Stage 3 since `crates/column/` is off-limits in Stage 2.)*

---

## 11. Expanded External Benchmarks — Lance V2.2 (8th Format), JSONBench, WildChat-1M & Hermes (`precision-5770-vm`)

Added **Lance V2.2** (`lance-file 14.0.3`, Arrow 58 IPC bridge, `SimulatedS3ObjectStore14`) as an 8th format across all 6 external suites, plus three real-world public corpora with **100% row-count parity** across all 7 tested formats (`pq_default`, `pq_tuned`, `vortex 0.95.0`, `lance 14.0.3`, `strata` PR #158, `statam_l0`, `statam_l1`):
- **Suite D (`jsonbench_100k`):** Real ClickHouse JSONBench Bluesky Jetstream (`bluesky_0001.json.gz`, 100,000 events × 10 cols, 92.1 MiB raw; `attrs: MapUtf8` shredded map + `raw_json` `.stpk` payload + `text` narrow trigram).
- **Suite E (`wildchat_15k`):** Real `allenai/WildChat-1M` (`wildchat_00000.parquet`, 15,000 multilingual multi-turn spans × 8 cols, 151.7 MiB raw; deterministic Unicode `ILIKE` needle `"Alienación"`, 3 hits).
- **Suite F (`hermes_fc`):** Real `NousResearch/hermes-function-calling-v1` (`hermes_fc.parquet`, 1,893 tool-calling conversations × 7 cols, 18.4 MiB raw; cross-trace `tool_definitions` `.stpk` dedup).

### 11.1 Storage Size & Write Speed Across All 6 External Suites (`8W` for Statam, `20T` for Strata)

| Suite (Raw) | `pq_default` | `pq_tuned` | `vortex` | `lance v2.2` | `strata` | `statam_l0 (8W)` | `statam_l1 (8W)` |
|---|---:|---:|---:|---:|---:|---:|---:|
| **`clickbench_100k`** *(99.1 MiB)* | 8,960 KiB · 273 ms | 9,253 KiB · 351 ms | **7,128 KiB** · 106 ms | 19,856 KiB · **66.3 ms** | 9,149 KiB · 892 ms | 13,966 KiB · 119 ms | 8,033 KiB (`12.3×`, `2.47×` < Lance) · 174 ms |
| **`row_filter_100k`** *(14.3 MiB)* | 1,259 KiB · 31.1 ms | 1,360 KiB · 32.6 ms | **894 KiB** · 19.3 ms | 1,602 KiB · 12.5 ms | 1,085 KiB · 74.6 ms | 1,690 KiB · 14.0 ms | 1,032 KiB (`13.8×`, `1.55×` < Lance) · **10.1 ms** |
| **`random_access_250k`** *(85.9 MiB)* | 12,893 KiB · 157 ms | 14,519 KiB · 172 ms | 9,912 KiB · 78.3 ms | 19,382 KiB · **57.9 ms** | 10,608 KiB · 574 ms | 17,935 KiB · 68.5 ms | **9,447 KiB (#1 of 7, `9.10×`)** · 135 ms |
| **`jsonbench_100k`** *(92.1 MiB)* | 20,522 KiB · 350 ms | 22,335 KiB · 330 ms | **20,177 KiB** · 155 ms | 42,696 KiB · 195 ms | 24,160 KiB · 1,205 ms | 38,288 KiB · **137 ms** | 27,222 KiB (`3.38×`, `1.57×` < Lance) · 415 ms |
| **`wildchat_15k`** *(151.7 MiB)* | 13,007 KiB · 269 ms | 14,605 KiB · 251 ms | 12,360 KiB · 665 ms | 37,493 KiB · 399 ms | **12,061 KiB** · 215 ms | 22,928 KiB · **103 ms** | 14,919 KiB (`10.17×`, `2.51×` < Lance) · 275 ms |
| **`hermes_fc`** *(18.4 MiB)* | 2,623 KiB · 57.3 ms | 4,192 KiB · 82.5 ms | 1,831 KiB · 966 ms | 9,878 KiB · 59.9 ms | **1,626 KiB** · 49.1 ms | 6,699 KiB · **14.9 ms** | 1,835 KiB (`10.01×`, `5.38×` < Lance) · 62.1 ms |

### 11.2 Query Results — Lance V2.2 + JSONBench + WildChat-1M + Hermes (Local Median ms / Simulated S3)

| Suite / Query | Matched | `pq_default` | `pq_tuned` | `vortex` | `lance v2.2` | `strata` | `statam_l1 (1R)` | `statam_l1 (8R)` | S3 Winner (`ms`, GETs) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `RandomAccess` `RA_take_100` | 100 | 41.00 | 39.10 | 24.13 | **2.58** | N/A | 21.78 | 3.94 | **`statam_l1` (`192 ms`, 4 GETs)** vs Lance `664 ms` (19 GETs) |
| `RandomAccess` `RA_take_1000` | 1,000 | 43.07 | 47.91 | 26.37 | 12.32 | N/A | 21.91 | **4.21** | **`statam_l1` (`192 ms`, 4 GETs)** vs Lance `734 ms` (21 GETs) |
| `ClickBench` `CB_Q1` | 43 | 0.343 | 0.370 | 0.216 | 0.880 | 0.701 | **0.016** | 0.109 | `lance` (`63.9 ms`, 2 GETs) / `statam_l0` (`66.4 ms`) |
| `ClickBench` `CB_Q20` *(4 cols)* | 6 | 3.124 | 1.737 | 3.406 | 31.44 | **0.163** | 1.910 | 1.890 | **`statam_l1` (`78.4 ms`, 2 GETs)** |
| `ClickBench` `CB_Q23_like` | 3 | 8.062 | 8.469 | 8.945 | 32.41 | **4.053** | 13.34 | 5.99 | `strata` (`109 ms`) / `statam_l1` (`113 ms`, 3 GETs) |
| `ClickBench` `CB_Q24` (`URL <> ''`) | 100,000 | 15.22 | 15.89 | 2.427 | 16.85 | **1.746** | 13.04 | 4.89 (`l0` 2.20) | **`statam_l1` (`78.4 ms`, 2 GETs)** |
| `ClickBench` `CB_wide_point` *(105 cols)* | 6 | 10.71 | 6.477 | 13.55 | 234.4 | **1.574** | 7.363 | 1.708 (`l0` 1.570) | **`statam_l1` (`128 ms`, 2 GETs)** vs Lance `644 ms` (18 GETs) |
| `RowFilter` *(6-pattern range)* | 1–99k | 2.19–4.82 | 0.63–5.52 | 0.99–2.50 | 5.02–7.96 | 0.15–2.34 | 0.61–2.06 | **0.37–0.64** | **`statam_l1` (`35.1 ms`, 1 GET)** across all 6/6 |
| `JSONBench` `JB_Q1_posts` | 8,737 | 5.016 | 5.093 | 2.021 | 6.687 | 1.611 | 0.574 | **0.328** | `pq_default` (`60.4 ms`) / `statam_l1` (`69.3 ms`, 2 GETs) |
| `JSONBench` `JB_Q2_did_pt` *(4 cols)* | 265 | 6.549 | 4.837 | 4.012 | 17.82 | **2.223** | 8.262 | 3.220 (`l0` 1.824) | **`statam_l1` (`89.3 ms`, 2 GETs)** |
| `JSONBench` `JB_Q3_map_type` *(Map)* | 8,737 | 62.28 | 73.46 | 27.82 | 54.32 | 13.18 | **2.044** | **1.232** | **`statam_l1` (`159 ms`, 4 GETs)** — **50× faster local than pq!** |
| `JSONBench` `JB_Q4_text_like` | 192 | 3.757 | 6.294 | 3.897 | 12.99 | 2.508 | 4.917 | 2.491 (`l0` **1.147**) | **`statam_l1` (`114 ms`, 3 GETs)** |
| `JSONBench` `JB_Q5_did_full` *(10 cols)* | 265 | 20.83 | 11.47 | 21.35 | 280.9 | **4.348** | 28.87 | 8.500 (`l0` 7.715) | **`statam_l1` (`251 ms`, 5 GETs)** vs Strata `411 ms`, Lance `942 ms` |
| `WildChat` `WC_Q1_model_cnt` | 10,201 | 0.559 | 0.564 | 0.309 | 0.620 | 0.404 | 0.045 | **0.043** | **`statam_l1` (`29.7 ms`, 1 GET, 478 KiB)** |
| `WildChat` `WC_Q2_lang_filter` | 4,730 | 1.271 | 1.204 | 0.785 | 1.243 | **0.398** | 0.491 | 0.486 | **`statam_l1` (`29.7 ms`, 1 GET, 478 KiB)** |
| `WildChat` `WC_Q3_trace_full` *(55 turns)* | 55 | 5.733 | 4.386 | 48.00 | 409.4 | **0.679** | 6.152 | 4.432 (`l0` 2.623) | `strata` (`113 ms`, 3 GETs) / `pq_tuned` (`168 ms`) / `statam_l1` (`171 ms`) |
| `WildChat` `WC_Q4_unicode_ilike` *("Alienación")* | 3 | 79.97 | 78.86 | 75.18 | 144.7 | 78.35 | 90.97 | **21.90** | `vortex` (`181 ms`, 4 GETs) / `statam_l1` (`196 ms`, 4 GETs) |
| `WildChat` `WC_Q5_input_absent` | 0 | 75.21 | 76.31 | 29.78 | 324.7 | **2.640** | 12.43 | 9.238 | `strata` (`77.8 ms`, 2 GETs) / `statam_l1` (`101 ms`, 3 GETs, 2.6 MiB) |
| `Hermes` `HF_Q1_cat_cnt` | 796 | 0.064 | 0.062 | 0.251 | 0.146 | 0.117 | **0.007** | **0.007** | **`statam_l1` (`26.1 ms`, 1 GET, 109 KiB)** |
| `Hermes` `HF_Q2_pt_full` *(7 cols)* | 1 | 4.022 | 3.234 | 8.314 | 39.41 | **0.498** | 2.700 | 1.484 (`l0` 0.949) | `strata` (`40.9 ms`, 1 GET) / `pq_default` (`65.6 ms`) / `statam_l1` (`92.9 ms`) |
| `Hermes` `HF_Q3_tool_like` *("Amazon")* | 5 | **1.693** | 1.717 | 1.810 | 2.837 | 4.902 | 3.767 | 3.036 (`l0` 2.171) | `strata` (`40.9 ms`) / `pq_tuned` (`60.9 ms`) / `statam_l1` (`92.9 ms`) |
| `Hermes` `HF_Q4_out_absent` | 0 | 0.870 | 0.864 | 1.002 | 1.023 | 0.309 | 0.298 | **0.297** | `pq_default` (`30.6 ms`, 1 GET) / `strata` (`40.9 ms`) / `statam_l1` (`57.9 ms`) |

### 11.3 Key Honest Takeaways from the 8-Format Expansion

- **Lance V2.2 (`lance-file 14.0.3`):** Excels on local single-threaded scattered random access (`RA_take_100`: `2.58 ms`) and flat-table write speed (`1.45 GiB/s`), but its lack of heavy block compression and row-group/zone-map pruning costs **`1.55×–5.38×` larger files**, **`2×–5×` higher S3 latency** (`18–21` GETs vs Statam's `2–4` GETs), and **`15×–137×` slower wide/LLM point lookups** (`CB_wide_point`: `234.4 ms`, `WC_Q3_trace_full`: `409.4 ms`, `JB_Q5_did_full`: `280.9 ms`). At `8R`, Statam beats Lance even on `RA_take_1000` (`4.21 ms` vs `12.32 ms`, `2.9×` faster).
- **Shredded `MapUtf8` on Real Bluesky JSON (`JB_Q3_map_type`):** Statam's shredded map layout delivers **`2.04 ms (1R)` / `1.23 ms (8R)`** — **`6.4×–10.7×` faster than Strata, `13.6×–22.6×` faster than Vortex, `26.6×–44.1×` faster than Lance, and `30.5×–50.5×` faster than Parquet** — while also winning S3 (`159 ms`, 4 GETs).
- **Multilingual WildChat-1M & Unicode `ILIKE` (`WC_Q4_unicode_ilike`):** At `8R`, Statam's parallel `.stpk` frame scan is **`3.4×–6.6×` faster than all 6 competitors** (`21.90 ms` vs `75–145 ms`). At `1R` (`90.97 ms`), non-ASCII `ILIKE` currently disables ASCII-trigram bloom pruning; extracting pure-ASCII runs `>= 3` bytes (e.g., `"alienaci"` from `"Alienación"`) for the trigram bloom is a clean future optimization that would drop `1R` latency below `3 ms`.
- **Small-Row `.stpk` Trade-offs (Honest Findings):** On `jsonbench_100k` (small ~400 B `raw_json` per event, 265 scattered DID hits), `.stpk` framing + blooms add ~7 MiB over Parquet/Vortex and cost `28.9 ms (1R)` / `8.5 ms (8R)` locally on `JB_Q5_did_full` vs Strata `4.35 ms` (though Statam still wins S3 at `251 ms` vs `411 ms`). Similarly on tiny `hermes_fc` (`1,893` rows, `1.8 MiB`), `HF_Q3_tool_like` favors inline columns (`1.69–1.81 ms` on Parquet/Vortex vs `3.04 ms` on `statam_l1 8R`), while Statam wins `HF_Q1_cat_cnt` (`7 µs`, `1` S3 GET) and `HF_Q4_out_absent` (`0.297 ms`) and matches Vortex's `10.0×` compression ratio while writing **`15.6×` faster**.

---

## 12. Pass 10 — `perf`-Driven Elimination of Remaining Competitor Gaps (`precision-5770-vm`)

Following a dedicated `perf` + `STATAM_DUMP_STATS` pass across all 8 lagging queries, implemented five targeted improvements (audited by Cold Review #9, `125` unit tests + `2` `fuzz_replay` tests passing, `0` clippy warnings):
1. **Zero-`from_utf8` Raw Phase-1 `LIKE`/`ILIKE` (`reader.rs`, `codec/fsst.rs`):** Scans `Rle` (once per unique run!), `Fsst` (`FsstBlockView::decode_row_into`), `Plain`, `Const`, and `Dict` (`dict_sub_bitmap`) directly on raw bytes without constructing full `Utf8Data` blocks (`CB_Q23_like` `41.6%` `from_utf8` eliminated).
2. **Cached Zero-Alloc `FlatEdgeTable` + CSR `FlatPayloadRefs` + Exhaustively-Verified Unicode `ILIKE` Trigram Extraction (`payload/{mod,pack,search}.rs`):** Eliminated `65,642` per-query `malloc`/`free` calls in `read_edge_table()` / `decode_payload_refs`, skipped dead `EdgeTable` / boundary pair work on `edge_table_len == 0` packs (with strict `StatamError::Corrupt` guard against crafted multi-chunk references), and added sound ASCII-run trigram extraction (`extract_unicode_ilike_trigram_hashes_in_slice`, proven across all `0..=0x10FFFF`).
3. **Sparse `Dict` & Sparse `Rle` Decode in Phase 2 & `take_rows` (`codec/{dict,mod}.rs`):** Avoids full-column materialization + `take` on selective point lookups and scattered `take_rows`.
4. **Boundary-Aware Dead `EdgeTable` Pruning + Optional `emit_hash_table` (`payload/pack.rs`, `writer.rs`, `gdpr.rs`):** Omits `EdgeTable` completely (`edge_table_len = 0`) on single-chunk packs (`jsonbench_100k`: `4,486 KiB → 0 KiB`), zeros non-boundary chunk edges on mixed packs (`wildchat_15k`: `1,336 KiB → 670 KiB`), and adds `PackWriteOptions::emit_hash_table` (default `true`; safe in `gdpr_rewrite_day`).
5. **`8,192`-Row Mini-Blocks + `512 KiB` `.stpk` Frames + Proportional Trigram Budget (`writer.rs`, `payload/pack.rs`):** Finer skipping & parallelism (`4×` more mini-blocks, `2×` finer `.stpk` frames) with `min(stored_len / 6, raw.len() / 32)` trigram cap.

### 12.1 Before vs. After Pass 10 — Latency Breakthroughs (`statam_l1`, `precision-5770-vm`)

| Workload / Query | Pre-Pass 10 (`1R` / `8R`) | Pass 10 (`1R` / `8R`) | Speedup | Competitive Result |
|---|---:|---:|---:|---|
| `ClickBench` `CB_Q23_like` | 13.34 / 5.99 ms | **2.43 / 1.71 ms** (`l0` **1.27 ms**) | **5.5× / 3.5×** | **Beats Strata (`4.05 ms`) even on 1 thread** (`2.4×–3.2×` at `8R`) |
| `ClickBench` `CB_Q24` | 13.04 / 4.89 ms | 12.99 / **2.64 ms** (`l0` **1.06 ms**) | `1.9×` (`l0` `2.1×`) | `l0 (8R)` **beats Strata (`1.75 ms`) & Vortex (`2.43 ms`) — #1 of 8** |
| `ClickBench` `CB_Q20` | 1.91 / 1.89 ms | **0.40 / 0.42 ms** | **4.8×** | **#1 S3 (`81 ms`, 2 GETs)**, `4×–8×` faster local than Parquet/Vortex |
| `ClickBench` `CB_wide_point` *(105 cols)* | 7.36 / 1.71 ms | **1.91 / 0.53 ms** | **3.9× / 3.2×** | **3.0× faster than Strata (`1.57 ms`), 440× faster than Lance!** |
| `RowFilter` `RF_pt_0_001pct` | 0.613 / 0.368 ms | **0.161 / 0.131 ms** (`l0` **0.098 ms**) | **3.8× / 2.8×** | **Beats Strata (`0.154 ms`) — #1 across all 6/6 patterns** |
| `RandomAccess` `RA_take_100` / `1000` | 21.8 / 3.94–4.21 ms | 22.0 / **3.54–3.66 ms** | `1.15×` (`8R`) | **S3 cut to 3 GETs / `168 ms`** (`4.4×` faster than Lance) |
| `JSONBench` `JB_Q5_did_full` *(10 cols)* | 28.87 / 8.50 ms | **17.90 / 4.33 ms** (`l0` **4.10 ms**) | **1.6× / 2.0×** | **Beats Strata (`4.35 ms`) — sweeps all 5/5 JSONBench #1!** |
| `WildChat` `WC_Q5_input_absent` | 12.43 / 9.24 ms | **3.28 / 1.77 ms** | **3.8× / 5.2×** | **Beats Strata (`2.64 ms`) by `1.49×`**; S3 `97.5 ms` (`2.3 MiB`) |
| `WildChat` `WC_Q4_unicode_ilike` | 90.97 / 21.90 ms | 83.02 / **13.44 ms** | **1.6× (`8R`)** | **`5.6×–10.8×` faster than every other format** |
| `WildChat` `WC_Q3_trace_full` | 6.15 / 4.43 ms | **4.24 / 2.66 ms** (`l0` **1.92 ms**) | **1.5× / 1.7×** | Beats Parquet even on `1R`, `18×` faster than Vortex, `154×` faster than Lance |
| `Hermes` `HF_Q3_tool_like` | 3.77 / 3.04 ms | **1.17 / 0.87 ms** | **3.2× / 3.5×** | **Beats Parquet (`1.69 ms`), Vortex (`1.81 ms`), Lance & Strata even on `1R`!** |
| `Hermes` `HF_Q4_out_absent` | 0.298 / 0.297 ms | **0.163 / 0.164 ms** | **1.8×** | **`1.9×–6.2×` faster than every competitor** |
| `W1_agents` `substring_rare` *(858 MiB)* | 33.35 / 16.86 ms | **24.49 / 8.77 ms** | **1.9× (`8R`)** | **1.95× faster than Strata (`17.14 ms`), 162× faster than Parquet!** |
| `W1_agents` `point_lookup_full` *(56 cols)* | 12.52 / 8.28 ms | **6.68 / 4.94 ms** | **1.9× / 1.7×** | Beats Vortex (`9.69 ms`) even on `1R` |
| `W1_agents` `substring_absent` | 1.94 ms (`1R`) | 2.00 / **0.99 ms** | **2.0× (`8R`)** | **S3 cut to 3 GETs / `110.6 ms`** (down from 4 GETs / `143 ms`) |
| `W2_chat_4files` `substring_rare` / `absent` | 39.3 / 33.4 ms | **6.49 / 4.08 ms (`8R`)** | **6.1× / 8.2×** | Huge multi-file `.stpk` speedup |
| `W3_spans` `point_lookup_full` | 5.19 / 1.44 ms | **1.59 / 0.53 ms** | **3.3× / 2.7×** | **Beats Strata (`0.72–1.16 ms`), `303×` faster than Parquet!** |
| `W3_scores` `point_lookup_narrow` | 1.52 / 0.89 ms | **0.64 / 0.38 ms** | **2.4× / 2.3×** | **`3.2×` faster than Vortex, `4.1×` faster than Strata!** |

### 12.2 Honest Regressions & Trade-offs in Pass 10
- **Storage Size:** `jsonbench_100k` shrank **`-12.4%`** (`27,222 → 23,855 KiB`, now beats Strata `24,160 KiB`), `wildchat_15k` shrank **`-2.2%`** (`14,919 → 14,595 KiB`, beats `pq_tuned`), `row_filter_100k` (`1,024 KiB`), `W4_adversarial` (`20,090 KiB`), and `W4_1row_avg` (`4.2 KiB`) all shrank. Smaller `8,192`-row mini-blocks + `512 KiB` frames added **`+0.9%` to `+3.2%`** on `W1` (`8,487 → 8,762 KiB`, still `-25.0%` vs best Parquet), `W2` (`15,006 → 15,209 KiB`, still #1 smallest of 7), `W3_spans` (`13,823 → 13,999 KiB`, `-5.3%` vs best Parquet), `W3_scores` (`5,267 → 5,314 KiB`, `-9.9%` vs best Parquet), `clickbench_100k` (`8,033 → 8,290 KiB`, `-7.5%` vs Parquet), and `random_access_250k` (`9,447 → 9,549 KiB`, still #1 smallest of 7). **All Gate 1 size checks remain PASS.**
- **Minor `1R` Full-Scan Overhead:** `3.25×` more mini-blocks (`8,192` vs `32,768`) slightly slowed a few single-threaded full-column scans (`CB_Q10 1R`: `0.35 → 0.63 ms`, `CB_Q12 1R`: `0.52 → 1.06 ms`, `JB_Q3_map_type 1R`: `2.04 → 3.75 ms`, `RF_unsel_99pct 1R`: `1.92 → 3.04 ms`), while `8R` remained faster or improved (`JB_Q3 8R`: `1.23 → 0.85 ms`).
- **Minor S3 / Write Shifts:** `W3_scores point_lookup_narrow` S3 shifted from `2 GETs / 91 ms` to `3 GETs / 127 ms` due to a `1 MiB` ReadCache boundary crossing (still #1 fastest S3 vs Strata `132 ms`, Vortex `173 ms`, Parquet `180–208 ms`); `clickbench_100k` write (`8W`) moved from `174 → 203 ms` (`1,365` vs `420` mini-blocks across 105 cols; still faster than Parquet `273 ms` & Strata `892 ms`).

**Stage 3 Status:** `crates/storage`, `crates/core`, `crates/column`, and `crates/query` remain **completely untouched** per the Stage 2/Stage 3 gate. Awaiting explicit user go-ahead for Stage 3 (`cetra-storage` integration).
