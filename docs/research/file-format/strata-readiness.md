# Strata format readiness

Date: 2026-11-25. Where Strata stands against Parquet and Vortex, whether it is production ready, and whether a new query engine that replaces DataFusion should be built on it. The rule for that decision: use Strata only if it is faster than both.

## Verdict: not yet

Strata is not faster than both formats today, and it is not production ready.
- It beats both on the trace workloads it was designed for: lookups by trace ID, substring search in prompts, and storing repetitive text.
- It beats Parquet on most analytical work.
- On general analytical scans, decoding, writes and random access, it is clearly slower than Vortex.

## Analytical query suites

Twelve suites from bench.vortex.dev, run through DataFusion 55 on the same machine (ovh-us) and the same data. Each ratio is the geometric mean of the per-query medians. Below 1 means Strata is faster.

| Suite | Strata / Vortex | Strata / Parquet | Vortex / Parquet |
|---|---:|---:|---:|
| vortex-queries | **0.74** | 0.44 | 0.59 |
| Appian | 1.02 | 0.98 | 0.97 |
| StatPopGen | 1.38 | 0.60 | 0.43 |
| TPC-H SF10 | 1.71 | 0.92 | 0.54 |
| TPC-H SF1 | 1.82 | 0.76 | 0.41 |
| Polar Signals | 1.84 | 0.20 | 0.11 |
| TPC-DS SF1 | 1.90 | 1.18 | 0.62 |
| ClickBench | 2.01 | 1.24 | 0.62 |
| FineWeb | 2.07 | 0.60 | 0.29 |
| Nested TPC-H SF1 | 2.42 | 0.92 | 0.38 |
| JOB | 4.35 | 1.55 | 0.35 |
| Sorted ClickBench | 5.10 | 1.56 | 0.26 |

Strata beats Parquet on 7 of the 12 suites. Against Vortex it is faster on one suite, ties on one, and is 1.4 to 5 times slower on the rest.

Every Strata answer matches Parquet's, except on queries whose answers include tied rows. On those queries Vortex differs from Parquet as well.

These numbers are from the committed code. Later work on top-k queries, not yet committed, changed three ClickBench queries:

| Query | Strata before | Strata after | Vortex |
|---|---:|---:|---:|
| q23 | 8.4 s | 0.31 s | 0.23 s |
| q24 | 0.80 s | 0.034 s | 0.029 s |
| q26 | 0.79 s | 0.034 s | 0.080 s |

Those three queries are most of Strata's time on sorted ClickBench, so that suite will improve a lot; the others will move little. A full A/B of that work is still running.

## Format-level numbers

These matter most for a new engine, because they don't depend on DataFusion. The first table is compress-bench, as geometric means over 24 datasets.

| Measure | Strata / Vortex | Strata / Parquet |
|---|---:|---:|
| Read time (decompress) | 2.28 | 0.77 |
| Write time (compress) | 2.12 | 2.32 |
| File size | 1.07 | 1.15 |

The second table is random-access-bench, which takes rows by index:

| Dataset and access pattern | Parquet | Vortex | Strata |
|---|---:|---:|---:|
| taxi, correlated rows | 25.6 ms | 0.48 ms | 1.48 ms |
| taxi, uniform rows | 41.1 ms | 3.1 ms | 15.6 ms |
| feature vectors, correlated | 10.3 ms | 0.16 ms | 2.33 ms |
| feature vectors, uniform | 117.8 ms | 8.4 ms | 34.2 ms |
| nested lists, correlated | 6.0 ms | 0.14 ms | 0.32 ms |
| nested structs, uniform | 36.4 ms | 0.40 ms | 2.76 ms |

On random access Strata is 3 to 20 times faster than Parquet, and 2 to 15 times slower than Vortex.

## Where Strata beats both

These results are for the LLM-observability workload Strata was designed for. They come from the research benchmarks on synthetic traces and real SWE-agent data ([README.md](README.md) § Strata v2 results).

| Workload | Strata | Against Parquet | Against Vortex |
|---|---|---|---|
| Warm full-trace lookup | 260–392 KiB read, 4.8 ms | 2.4–6.7× fewer bytes | Vortex reads 66–80 MB |
| `%needle%` search in prompts | 5-gram index per frame | 25× faster | 10× faster |
| Repetitive payload size | dedup across rows | 0.56× the size | smaller |
| 16-byte IDs, `map['key']` filters | stored and pushed down | supported | Vortex can't |

## On object storage (S3)

These runs used Cloudflare R2 and our own trace benchmark (fmtbench) on six LLM-trace datasets, from [real-data.md](real-data.md) § Real S3 and § Real S3 from Virginia.
- **When:** 2026-11-21 and 22, on the Strata of those days, before this week's scan changes. Those changes were to scan and decode speed, so lookups should be about the same; they haven't been rerun.
- **Not covered:** the bench.vortex.dev analytical suites were never run on S3.

| Machine and bucket | Time to first byte (median) | Throughput per request |
|---|---:|---:|
| Helsinki, EU bucket | 59–69 ms | 99–118 MB/s |
| Virginia (ovh-us), US bucket | 44–50 ms | 101–113 MB/s |

**Cold trace lookup from Helsinki** (ms per trace, Strata with its default settings):

| Dataset | Best Parquet | Strata | Vortex |
|---|---:|---:|---:|
| light | 536 | **458** | 1,039 |
| payload | 553 | **468** | 1,121 |
| swe | **375** | 383 | 702 |
| openswe | 406 | **393** | 1,281 |
| toucan | 481 | **425** | 1,015 |
| wildchat | 598 | **481** | 1,449 |

**Every query from Virginia** (ms, Strata / best Parquet / Vortex, mean of two alternating rounds; Strata with its default settings):

| Query | light | payload | swe | openswe | toucan | wildchat |
|---|---|---|---|---|---|---|
| Recent LLM spans (`list_llm_1h`) | **227** / 577 / 393 | **289** / 309 / 397 | 249 / 218 / **193** | 258 / 239 / **185** | **183** / 309 / 290 | **235** / 318 / 316 |
| Model rollup by day | **215** / 327 / 451 | 166 / 176 / **160** | **130** / 140 / 156 | 141 / **138** / 150 | 188 / **156** / 157 | 273 / **207** / 222 |
| Full trace lookup | **140** / 263 / 807 | **212** / 227 / 823 | 168 / **151** / 360 | 195 / **151** / 910 | 214 / **181** / 524 | **200** / 226 / 1,002 |
| Substring search in inputs | – | **369** / 1,273 / 1,119 | **285** / 497 / 390 | **237** / 564 / 1,658 | **444** / 913 / 1,100 | **436** / 2,161 / 2,233 |
| Full payload scan | – | **1,810** / 2,558 / 2,431 | 513 / 551 / **474** | **507** / 677 / 1,708 | **1,411** / 1,805 / 1,766 | 5,247 / 4,372 / **4,070** |
| Cold lookup | **383** / 412 / 897 | 402 / **384** / 926 | 382 / **325** / 525 | 368 / **299** / 1,151 | 388 / **341** / 777 | 440 / **354** / 1,214 |

- **Searches:** Strata leads all five datasets that have them, 2–5× ahead of Parquet.
- **Scans:** Strata leads most. Its wildchat payload scan is slowest, and varied between rounds (3.5 s and 7.0 s).
- **Cold lookups with default settings:** Parquet led on five datasets. Strata sends many small GETs per wave, and a wave waits for its slowest GET.
- **Cold lookups with three read settings for object storage:**
  - the settings: the catalog's exact tail length at open, and a 256 KiB merge gap for lookup reads, capped at 1 MiB;
  - the result: lookups 8–32% faster, Strata ahead of Parquet on four datasets and within 5% on the other two;
  - searches and scans don't change.

  These settings are off by default; they change only how files are read, not the files.
- **Vortex:** 2–6× slower on lookups everywhere; it reads 30–250 MB per lookup.

On S3, Strata wins searches and most lookups against both formats. That is the trace workload it was designed for.

## Production readiness

- **Unstable format (blocker).** The format version is 0, and the encoding changed this week (LZ4 was removed). Nothing guarantees that files written today will open next month.
- **Research code (blocker).** It has one Rust implementation, in `docs/research/file-format/strata`. There is no reader in another language and no production use.
- **Slow writes and high memory (gap).**
  - Writes are 2.1× slower than Vortex and 2.3× slower than Parquet.
  - A full payload scan peaks at about 2.4× Parquet's memory.
- **Testing (in place).**
  - Fuzz targets, model tests and property tests all pass.
  - `unsafe` is limited to two decode kernels.
  - It has never run against real traffic. On object storage it was measured on R2 (§ On object storage), but never run for long or under load.

## What this means for a new query engine

Many of Strata's current wins and losses come from its DataFusion integration rather than from the format. This week's top-k fix is an example. A new engine rebuilds that layer anyway, and can give the same techniques to Parquet or Vortex readers.

What carries over to the new engine is the format itself:
- **Raw decode speed:** Vortex is about 2.3× ahead.
- **Compute on compressed data:** Vortex is more mature.
- **Pruning for traces** (sort-key index, bloom filters, postings, n-gram frame index): Strata is ahead.
- **Wide, repetitive values:** Strata is clearly ahead.

## Recommendation

- **If the rule stays "faster than both on everything":** don't build the engine on Strata now. Vortex is the stronger general columnar format, but it is weak on trace lookups, payloads and 16-byte IDs.
- **If cetra's real queries are mostly** trace lookups, span-tree loads and searches over prompts and completions, Strata is already the best of the three for them. Then the right test is a benchmark of cetra's real query mix, not ClickBench or TPC-H.
- **Making Strata also win on narrow scans** needs dedicated work: decode kernels at Vortex's speed, compute on compressed data, and faster writes. That is weeks of work, not days.
- **Before choosing:** measure the new engine's real query mix on all three formats at the format level, with DataFusion left out.

## Sources

- **Machine:** ovh-us, running one benchmark at a time on an otherwise idle machine.
- **Harness:** bench.vortex.dev suites at Vortex `d7aa24e8f`, DataFusion 55.2, Parquet through arrow-rs.
- **Dates:** query and compress runs on 2026-11-25; random access on 2026-11-24; R2 runs on 2026-11-21 and 22.
- **Trace-workload figures:** [README.md](README.md) § Strata v2 results.
